use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use super::AgentManager;
use crate::agents::adapters::codex::{mapper, CodexClient, CodexDriver, CodexEvent};
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::*;
use crate::agents::ports::{agent::AgentPort, mirror::SessionMirrorPort};

struct Context {
    driver: Arc<CodexDriver>,
    mirror: Arc<MemoryMirror>,
    tx: broadcast::Sender<AgentDomainEvent>,
    stopped: Arc<Mutex<bool>>,
    threads: tokio::sync::Mutex<HashSet<String>>,
}

impl Context {
    async fn track(&self, thread: &str) -> bool {
        let mut threads = self.threads.lock().await;
        if threads.len() >= 4096 && !threads.contains(thread) {
            self.driver.client().stop();
            return false;
        }
        threads.insert(thread.into());
        true
    }
    fn emit(&self, mut event: AgentDomainEvent) {
        let stopped = self.stopped.lock().unwrap_or_else(|p| p.into_inner());
        if *stopped {
            return;
        }
        self.mirror.stamp_event(&mut event);
        let _ = self.tx.send(event);
    }

    async fn session(&self, thread: &Value, revision: u64) {
        let Some(id) = thread["id"].as_str() else {
            self.driver.client().stop();
            return;
        };
        let versions = self.driver.metadata_guard().await;
        if versions.get(id).copied().unwrap_or(0) != revision {
            return;
        }
        let Ok(mut info) = mapper::session(thread) else {
            self.driver.client().stop();
            return;
        };
        // Metadata was sampled by a concurrent read. Status notifications may
        // already have advanced the session (including turn completion); do not
        // turn an authoritative model refresh into a stale status rollback.
        if !self.mirror.is_placeholder(&info.asid).await {
            if let Some(current) = self.mirror.session_info(&info.asid).await {
                info.status = current.status;
            }
        }
        self.update_info(info).await;
    }

    async fn observe_runtimes(&self) {
        for (asid, expected) in self.mirror.runtime_sessions().await {
            if let Some(runtime) = self.driver.runtime(&asid.0, false).await {
                let usage = self.driver.cached_usage(&asid.0).await;
                if let Some((seq, info)) = self
                    .mirror
                    .update_runtime_decorated(&asid, expected, runtime, |info| {
                        if let Some(usage) = usage {
                            usage.apply(info);
                        }
                    })
                    .await
                {
                    self.emit(AgentDomainEvent::SessionUpdated {
                        asid,
                        info: Box::new(info),
                        seq,
                    });
                }
            }
        }
    }

    async fn observe_rollouts(&self) {
        for thread in self.driver.rollouts.sessions() {
            let asid = AgentSessionId(thread.clone());
            let Some(epoch) = self.mirror.observation_epoch(&asid).await else {
                continue;
            };
            let Some(result) = self.driver.rollouts.poll(&self.driver, &thread).await else {
                continue;
            };
            match result {
                Ok(update) => {
                    let Some(runtime) = self.driver.cached_runtime(&thread).await else {
                        continue;
                    };
                    if !self.driver.rollouts.current(&thread, update.generation) {
                        continue;
                    }
                    let usage = update
                        .usage
                        .as_ref()
                        .map(crate::agents::adapters::codex::usage::Usage::from_wire);
                    if self
                        .mirror
                        .apply_observation(
                            &asid,
                            epoch,
                            runtime,
                            update.items,
                            |info| {
                                if let Some(usage) = &usage {
                                    usage.apply(info);
                                }
                            },
                            |events| {
                                for event in events {
                                    self.emit(event.clone());
                                }
                            },
                        )
                        .await
                        .is_some()
                    {
                        if let Some(value) = update.usage {
                            let _ = self.driver.record_usage(&thread, &value).await;
                        }
                    } else {
                        self.driver.rollouts.fence(&thread, update.generation);
                    }
                }
                Err(_) => {
                    if let Some(runtime) = self.driver.cached_runtime(&thread).await {
                        if let Some((_, expected)) = self
                            .mirror
                            .runtime_sessions()
                            .await
                            .into_iter()
                            .find(|(id, _)| id == &asid)
                        {
                            if let Some((seq, info)) =
                                self.mirror.update_runtime(&asid, expected, runtime).await
                            {
                                self.emit(AgentDomainEvent::SessionUpdated {
                                    asid: asid.clone(),
                                    info: Box::new(info),
                                    seq,
                                });
                            }
                        }
                    }
                    self.emit(AgentDomainEvent::Resync {
                        asid,
                        reason: "codex_persisted_observation_refresh_required".into(),
                    });
                }
            }
        }
    }

    async fn update_info(&self, mut info: AgentSessionInfo) {
        self.driver.apply_usage(&mut info).await;
        if let Some(runtime) = self.driver.cached_runtime(&info.asid.0).await {
            runtime.apply(&mut info);
        }
        let asid = info.asid.clone();
        info.agent_id = self.driver.kind().into();
        if !self.track(&asid.0).await {
            return;
        }
        let seq = self.mirror.update_session(info.clone()).await;
        self.mirror.overlay_read_markers(&mut info).await;
        self.emit(AgentDomainEvent::SessionUpdated {
            asid,
            info: Box::new(info),
            seq,
        });
    }

    async fn ensure(&self, thread: &str) {
        if !self.track(thread).await {
            return;
        }
        let asid = AgentSessionId(thread.into());
        if !self.mirror.is_placeholder(&asid).await {
            return;
        }
        let _ = self.driver.refresh_metadata(thread).await;
    }

    async fn upsert(&self, asid: &AgentSessionId, items: Vec<TimelineItem>) {
        if items.is_empty() {
            return;
        }
        // A spawn starts without a receiver; its final item names exact child
        // rows. Replace that same producer's provisional rows, including when
        // the completed item arrives through turn/completed history replay.
        if items.iter().any(|item| {
            matches!(&item.part, AgentPart::Tool(call)
            if call.metadata.as_ref().is_some_and(|m| m["type"] == "collabAgentToolCall"))
        }) {
            if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
                let group = &items[0].message_id;
                let stale: Vec<_> = snapshot
                    .timeline
                    .iter()
                    .filter(|old| {
                        old.message_id == *group && !items.iter().any(|new| new.id == old.id)
                    })
                    .map(|old| old.id.clone())
                    .collect();
                if let Some((seq, ids)) = self.mirror.remove_timeline_items(asid, &stale).await {
                    self.emit(AgentDomainEvent::TimelineRemoved {
                        asid: asid.clone(),
                        ids,
                        seq,
                    });
                }
            }
        }
        let seq = self.mirror.upsert_timeline_items(asid, items.clone()).await;
        self.emit(AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items,
            seq,
        });
    }

    async fn set_status(&self, thread: &str, status: AgentSessionStatus) {
        let asid = AgentSessionId(thread.into());
        if let Some(seq) = self.mirror.update_status(&asid, status, None).await {
            self.emit(AgentDomainEvent::StatusChanged {
                asid,
                status,
                error: None,
                seq,
            });
        }
    }

    async fn resolve(&self, thread: &str, id: &str, form: bool) {
        let asid = AgentSessionId(thread.into());
        if form {
            let seq = self.mirror.resolve_form(&asid, id).await;
            self.emit(AgentDomainEvent::FormResolved {
                asid,
                form_id: id.into(),
                seq,
            });
        } else {
            let seq = self.mirror.resolve_permission(&asid, id).await;
            self.emit(AgentDomainEvent::PermissionResolved {
                asid,
                request_id: id.into(),
                seq,
            });
        }
    }

    async fn clear_requests(&self, thread: &str) {
        if let Some(snapshot) = self
            .mirror
            .get_snapshot(&AgentSessionId(thread.into()))
            .await
        {
            for request in snapshot.permissions {
                self.resolve(thread, &request.id, false).await;
            }
            for form in snapshot.forms {
                self.resolve(thread, &form.id, true).await;
            }
        }
    }

    async fn disconnected(&self) {
        let threads = self.threads.lock().await.clone();
        for thread in threads {
            if let Some(info) = self
                .mirror
                .session_info(&AgentSessionId(thread.clone()))
                .await
            {
                if info.status == AgentSessionStatus::Busy {
                    self.set_status(&thread, AgentSessionStatus::Unknown).await;
                }
            }
            // Expire locally, but do not advertise successful callback reply
            // resolution after an ambiguous/failed transport write. The resync
            // below is the authoritative public loss/unknown signal.
            let asid = AgentSessionId(thread.clone());
            if let Some(snapshot) = self.mirror.get_snapshot(&asid).await {
                let mut info = snapshot.info;
                info.model = None;
                self.update_info(info).await;
                for request in snapshot.permissions {
                    self.mirror.resolve_permission(&asid, &request.id).await;
                }
                for form in snapshot.forms {
                    self.mirror.resolve_form(&asid, &form.id).await;
                }
            }
        }
        self.emit(AgentDomainEvent::Resync {
            asid: AgentSessionId(String::new()),
            reason: "codex_connection_lost_outcome_unknown".into(),
        });
    }

    async fn event(&self, event: CodexEvent) {
        match event {
            CodexEvent::Attached { thread } => {
                if let Ok(info) = mapper::session(&thread) {
                    self.update_info(info).await;
                } else {
                    self.driver.client().stop();
                }
            }
            CodexEvent::Snapshot { thread, revision } => self.session(&thread, revision).await,
            CodexEvent::MetadataRefreshFailed { thread, revision } => {
                let versions = self.driver.metadata_guard().await;
                if versions.get(&thread).copied() == Some(revision) {
                    let asid = AgentSessionId(thread.clone());
                    if let Some(snapshot) = self.mirror.get_snapshot(&asid).await {
                        let mut info = snapshot.info;
                        info.model = None;
                        self.update_info(info).await;
                    }
                    self.emit(AgentDomainEvent::Resync {
                        asid,
                        reason: "codex_metadata_refresh_failed_resync_required".into(),
                    });
                }
            }
            CodexEvent::Disconnected => self.disconnected().await,
            CodexEvent::Resolved { thread, id, form } => self.resolve(&thread, &id, form).await,
            CodexEvent::Request { id, method, params } => {
                if let Some(request) = self.driver.permission(&id, &method, &params) {
                    self.ensure(&request.asid.0).await;
                    if !self.driver.client().is_ready() {
                        return;
                    }
                    let seq = self.mirror.add_permission(request.clone()).await;
                    self.emit(AgentDomainEvent::PermissionPending {
                        asid: request.asid.clone(),
                        request,
                        seq,
                    });
                } else if let Some(request) = self.driver.form(&id, &method, &params) {
                    self.ensure(&request.asid.0).await;
                    if !self.driver.client().is_ready() {
                        return;
                    }
                    let seq = self.mirror.add_form(request.clone()).await;
                    self.emit(AgentDomainEvent::FormPending {
                        asid: request.asid.clone(),
                        request,
                        seq,
                    });
                } else {
                    let _ = self.driver.client().reject_unsupported(&id).await;
                }
            }
            CodexEvent::Notification { method, params } => {
                self.notification(&method, &params).await
            }
        }
    }

    async fn notification(&self, method: &str, params: &Value) {
        if method == "thread/started" {
            self.session(&params["thread"], 0).await;
            return;
        }
        let Some(thread) = params["threadId"].as_str() else {
            return;
        };
        if !self.track(thread).await {
            return;
        }
        let asid = AgentSessionId(thread.into());
        // Ownership ends when the private server unloads the runtime. History
        // remains readable, but callbacks and the loaded marker are no longer live.
        if method == "thread/closed"
            || (method == "thread/status/changed" && params["status"]["type"] == "notLoaded")
        {
            self.driver.forget_thread(thread).await;
            self.clear_requests(thread).await;
            if let Some(snapshot) = self.mirror.get_snapshot(&asid).await {
                let mut info = snapshot.info;
                info.status = AgentSessionStatus::Unknown;
                self.update_info(info).await;
            }
            return;
        }
        // A resume snapshot can be superseded by the accepted turn's metadata
        // revision. Publish ownership before its first native timeline event,
        // even when that older metadata snapshot was correctly discarded.
        if let Some(expected) = self.mirror.non_native_runtime_seq(&asid).await {
            if let Some(runtime) = self.driver.cached_runtime(thread).await {
                if let Some((seq, info)) =
                    self.mirror.update_runtime(&asid, expected, runtime).await
                {
                    self.emit(AgentDomainEvent::SessionUpdated {
                        asid: asid.clone(),
                        info: Box::new(info),
                        seq,
                    });
                }
            }
        }
        match method {
            "thread/status/changed" => {
                self.set_status(thread, mapper::status(&params["status"]))
                    .await
            }
            "thread/tokenUsage/updated" => {
                if self
                    .driver
                    .record_usage(thread, &params["tokenUsage"])
                    .await
                    .is_err()
                {
                    return;
                }
                if self.mirror.is_placeholder(&asid).await {
                    self.ensure(thread).await;
                } else if let Some(info) = self.mirror.session_info(&asid).await {
                    self.update_info(info).await;
                }
            }
            "thread/name/updated" => {
                let _ = self.driver.refresh_metadata(thread).await;
            }
            "thread/deleted" => {
                self.driver.clear_usage(thread).await;
                self.clear_requests(thread).await;
                self.driver.forget_thread(thread).await;
                self.threads.lock().await.remove(thread);
                if let Some(snapshot) = self.mirror.get_snapshot(&asid).await {
                    let mut info = snapshot.info;
                    info.deleted = true;
                    self.mirror.remove_session(&asid).await;
                    self.emit(AgentDomainEvent::SessionUpdated {
                        asid,
                        info: Box::new(info),
                        seq: snapshot.seq + 1,
                    });
                }
            }
            "turn/plan/updated" | "turn/diff/updated" => {
                let Some(turn) = params["turnId"].as_str() else {
                    return;
                };
                let mut items = mapper::turn_update(thread, turn, method, params);
                let Ok(position) =
                    self.driver
                        .item_position(thread, turn, &format!(":event:{method}"))
                else {
                    return;
                };
                for item in &mut items {
                    item.message_id = mapper::order_group(thread, turn, position);
                }
                // Explanations can be removed by a subsequent plan update.
                if method == "turn/plan/updated" && items.len() == 1 {
                    let old = format!("{thread}/{turn}:event:{method}:1");
                    if let Some((seq, ids)) = self.mirror.remove_timeline_items(&asid, &[old]).await
                    {
                        self.emit(AgentDomainEvent::TimelineRemoved {
                            asid: asid.clone(),
                            ids,
                            seq,
                        });
                    }
                }
                self.upsert(&asid, items).await;
            }
            "turn/started" => {
                self.set_status(thread, AgentSessionStatus::Busy).await;
                // The acknowledged turn/start schedules its authoritative read.
                // Do not duplicate it or depend on experimental settings events.
            }
            "turn/completed" => {
                self.set_status(thread, mapper::status(&params["turn"]["status"]))
                    .await;
                self.clear_requests(thread).await;
                if let Some(turn) = params["turn"]["id"].as_str() {
                    if let Some(items) = params["turn"]["items"].as_array() {
                        for item in items {
                            if let Ok(items) = self.driver.ordered_items(thread, turn, item, 0) {
                                self.upsert(&asid, items).await;
                            }
                        }
                    }
                }
            }
            "serverRequest/resolved" => {
                if let Some(id) = self.driver.client().request_id(&params["requestId"]) {
                    if let Some(snapshot) = self.mirror.get_snapshot(&asid).await {
                        if snapshot.forms.iter().any(|f| f.id == id) {
                            self.resolve(thread, &id, true).await;
                        } else if snapshot.permissions.iter().any(|p| p.id == id) {
                            self.resolve(thread, &id, false).await;
                        }
                    }
                }
            }
            "item/started" | "item/completed" => {
                let Some(turn) = params["turnId"].as_str() else {
                    return;
                };
                let mut value = params["item"].clone();
                if value.get("status").is_none() && value.is_object() {
                    value["status"] = serde_json::json!(if method == "item/started" {
                        "inProgress"
                    } else {
                        "completed"
                    });
                }
                let Ok(mut items) = self.driver.ordered_items(
                    thread,
                    turn,
                    &value,
                    params["startedAtMs"]
                        .as_u64()
                        .or_else(|| params["completedAtMs"].as_u64())
                        .unwrap_or(0),
                ) else {
                    return;
                };
                for item in &mut items {
                    if let AgentPart::Tool(call) = &mut item.part {
                        if call.output.is_none() {
                            if let Some(old) = self.mirror.get_timeline_item(&asid, &item.id).await
                            {
                                if let AgentPart::Tool(old) = old.part {
                                    call.output = old.output;
                                }
                            }
                        }
                    }
                }
                self.upsert(&asid, items).await;
            }
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta" => {
                let (Some(turn), Some(item), Some(delta)) = (
                    params["turnId"].as_str(),
                    params["itemId"].as_str(),
                    params["delta"].as_str(),
                ) else {
                    return;
                };
                let message = mapper::message_id(thread, turn, item);
                let reasoning = method != "item/agentMessage/delta";
                let ordinal = if reasoning {
                    let index = if method == "item/reasoning/textDelta" {
                        "contentIndex"
                    } else {
                        "summaryIndex"
                    };
                    let Some(index) = params[index].as_u64().filter(|n| {
                        *n < crate::agents::adapters::codex::MAX_REASONING_PARTS as u64
                    }) else {
                        self.driver.client().stop();
                        return;
                    };
                    if method == "item/reasoning/textDelta" {
                        1_000_000 + index
                    } else {
                        index
                    }
                } else {
                    0
                };
                let id = if reasoning {
                    reasoning_item_id(&message, ordinal)
                } else {
                    text_item_id(&message, ordinal)
                };
                let Ok(position) = self.driver.item_position(thread, turn, item) else {
                    return;
                };
                let order_group = mapper::order_group(thread, turn, position);
                let old_len = self
                    .mirror
                    .get_timeline_item(&asid, &id)
                    .await
                    .map(|i| match i.part {
                        AgentPart::Text { text } | AgentPart::Reasoning { text, .. } => text.len(),
                        _ => 0,
                    })
                    .unwrap_or(0);
                if old_len + delta.len() > super::super::adapters::codex::client::MAX_FRAME_BYTES {
                    self.driver.client().stop();
                    return;
                }
                if let Some(seq) = self
                    .mirror
                    .append_text_delta(&asid, &id, &order_group, ordinal, delta, reasoning)
                    .await
                {
                    if let Some(item) = self.mirror.get_timeline_item(&asid, &id).await {
                        self.emit(AgentDomainEvent::TimelineUpsert {
                            asid,
                            items: vec![item],
                            seq,
                        });
                    }
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let (Some(turn), Some(item), Some(delta)) = (
                    params["turnId"].as_str(),
                    params["itemId"].as_str(),
                    params["delta"].as_str(),
                ) else {
                    return;
                };
                let id = tool_item_id(&mapper::message_id(thread, turn, item), item);
                if let Some(mut row) = self.mirror.get_timeline_item(&asid, &id).await {
                    if let AgentPart::Tool(call) = &mut row.part {
                        let old = call.output.as_ref().and_then(Value::as_str).unwrap_or("");
                        if old.len() + delta.len()
                            > crate::agents::adapters::codex::client::MAX_FRAME_BYTES
                        {
                            self.driver.client().stop();
                            return;
                        }
                        call.output = Some(Value::String(format!("{old}{delta}")));
                        self.upsert(&asid, vec![row]).await;
                    }
                }
            }
            // Unknown notifications carry no replay guarantee and are not
            // fabricated into timeline text or actionable approvals.
            _ => {}
        }
    }
}

impl AgentManager {
    pub fn connect_codex(
        client: Arc<CodexClient>,
        events: mpsc::Receiver<CodexEvent>,
        tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Self {
        Self::connect_codex_observed(client, events, tx, None)
    }

    pub(crate) fn connect_codex_observed(
        client: Arc<CodexClient>,
        mut events: mpsc::Receiver<CodexEvent>,
        tx: broadcast::Sender<AgentDomainEvent>,
        observer: Option<Arc<crate::agents::terminal_runtime::TerminalRuntime>>,
    ) -> Self {
        let endpoint = client.endpoint();
        let observed = observer.is_some();
        let mut driver = CodexDriver::new(client.clone());
        driver.observe_terminals(observer);
        let driver = Arc::new(driver);
        let mirror = Arc::new(MemoryMirror::for_agent("codex"));
        let stopped = Arc::new(Mutex::new(false));
        let context = Arc::new(Context {
            driver: driver.clone(),
            mirror: mirror.clone(),
            tx: tx.clone(),
            stopped: stopped.clone(),
            threads: tokio::sync::Mutex::new(HashSet::new()),
        });
        if observed {
            let context = context.clone();
            let cancel = client.cancel_token();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! { biased; _ = cancel.cancelled() => break, _ = tick.tick() => {} }
                    tokio::select! { biased; _ = cancel.cancelled() => break, _ = context.observe_runtimes() => {} }
                }
            });
        }
        if observed {
            let context = context.clone();
            let cancel = client.cancel_token();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! { biased; _ = cancel.cancelled() => break, _ = tick.tick() => {} }
                    tokio::select! { biased; _ = cancel.cancelled() => break, _ = context.observe_rollouts() => {} }
                }
            });
        }
        let cancel = client.cancel_token();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => { context.disconnected().await; break; }
                    event = events.recv() => match event {
                        Some(CodexEvent::Disconnected) | None => { context.disconnected().await; break; }
                        Some(event) => context.event(event).await,
                    }
                }
            }
        });
        let ready = client.clone();
        Self::assemble(
            driver,
            mirror,
            tx,
            endpoint.into(),
            Some("0.162.1".into()),
            Arc::new(move || ready.is_ready()),
            Arc::new(move || {
                *stopped.lock().unwrap_or_else(|p| p.into_inner()) = true;
                client.stop();
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::tests::{golden, Fixture};
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn persisted_observation_emits_metadata_before_redacted_rows_and_fences_rotation() {
        use std::io::Write;
        let home = std::env::temp_dir().join(format!("muqun-persisted-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let home = home.canonicalize().unwrap();
        let path = home.join("sessions/fixture.jsonl");
        let id = "00000000-0000-4000-8000-000000000001";
        std::fs::write(
            &path,
            format!("{}\n", json!({"type":"session_meta","payload":{"id":id}})),
        )
        .unwrap();
        let herdr = crate::test_support::FakeHerdr::start(vec![], None);
        herdr.set_agents(json!([{"pane_id":"w1:p1","terminal_id":"fixture-terminal","agent":"codex","agent_status":"working","shell_pid":42,"agent_session":{"kind":"id","agent":"codex","value":id}}]));
        let selected = path.clone();
        let mut fixture = Fixture::new_in_home(move |request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                let mut thread = golden()["thread"].clone();
                thread["id"] = json!(id);
                thread["status"] = json!({"type":"notLoaded"});
                thread["path"] = json!(selected);
                thread["historyMode"] = json!("paginated");
                json!({"thread":thread})
            }
            "thread/items/list" if request["params"]["turnId"] == "turn" => json!({"data":[{"turnId":"turn","startedAtMs":10,"item":{"id":"command","type":"commandExecution","command":"[REDACTED]","status":"inProgress","aggregatedOutput":"safe"}}],"nextCursor":null}),
            "thread/items/list" => json!({"data":[],"nextCursor":null}),
            _ => panic!("persisted observation must not send controls"),
        }, &home).await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(64);
        let manager = AgentManager::connect_codex_observed(
            fixture.client.clone(),
            fixture.events.take().unwrap(),
            tx,
            Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[
                herdr.session(),
            ])),
        );
        let asid = AgentSessionId(id.into());
        manager.sessions().get_snapshot(&asid).await.unwrap();
        let append = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":id,"turn_id":"turn","item":{"id":"command","type":"CommandExecution","command":["private raw command"]}}});
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{append}"
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5),async {
            let mut observed_metadata = false;
            loop {
                match events.recv().await.unwrap() {
                    AgentDomainEvent::SessionUpdated { info, .. } if info.runtime.as_ref().and_then(|r|r.transcript) == Some(TranscriptSource::Persisted) => observed_metadata = true,
                    AgentDomainEvent::TimelineUpsert { items, .. } => {
                        assert!(observed_metadata);
                        assert_eq!(items.len(),1);
                        assert!(matches!(&items[0].part,AgentPart::Tool(tool) if tool.input["command"] == "[REDACTED]" && tool.status == ToolCallStatus::Running));
                        break;
                    }
                    _ => {}
                }
            }
        }).await.unwrap();
        let replacement = path.with_extension("replacement");
        std::fs::copy(&path, &replacement).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AgentDomainEvent::Resync { reason, .. } = events.recv().await.unwrap() {
                    assert_eq!(reason, "codex_persisted_observation_refresh_required");
                    break;
                }
            }
        })
        .await
        .unwrap();
        let snapshot = manager.sessions().get_snapshot(&asid).await.unwrap();
        assert_eq!(
            snapshot.info.runtime.unwrap().transcript,
            Some(TranscriptSource::Snapshot)
        );
        assert_eq!(snapshot.timeline.len(), 1);
        assert!(snapshot.permissions.is_empty() && snapshot.forms.is_empty());
        fixture.client.stop();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(home.join("sessions")).unwrap();
        std::fs::remove_dir(home).unwrap();
    }

    #[tokio::test]
    async fn terminal_observation_updates_metadata_without_refetching_or_fabricating_forms() {
        let herdr = crate::test_support::FakeHerdr::start(vec![], None);
        let id = "00000000-0000-4000-8000-000000000001";
        let row = |status: &str| json!([{"pane_id":"w1:p1","terminal_id":"fixture-terminal","agent":"codex","agent_status":status,"shell_pid":42,"agent_session":{"kind":"id","agent":"codex","value":id}}]);
        herdr.set_agents(row("working"));
        let mut fixture = Fixture::new(move |request| match request["method"].as_str().unwrap() {
            "thread/read" => {
                let mut thread = golden()["thread"].clone();
                thread["id"] = json!(id);
                thread["status"] = json!({"type":"notLoaded"});
                json!({"thread":thread})
            }
            "thread/items/list" => json!({"data":[],"nextCursor":null,"backwardsCursor":null}),
            _ => panic!("observation must not control the provider"),
        })
        .await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(64);
        let manager = AgentManager::connect_codex_observed(
            fixture.client.clone(),
            fixture.events.take().unwrap(),
            tx,
            Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[
                herdr.session(),
            ])),
        );
        let asid = AgentSessionId(id.into());
        let first = manager.sessions().get_snapshot(&asid).await.unwrap();
        assert_eq!(first.info.status, AgentSessionStatus::Busy);
        while fixture.requests.try_recv().is_ok() {}
        herdr.set_agents(row("blocked"));
        let updated = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                if let AgentDomainEvent::SessionUpdated { info, .. } = events.recv().await.unwrap()
                {
                    if info.runtime.as_ref().and_then(|r| r.status)
                        == Some(TerminalRuntimeStatus::Blocked)
                    {
                        break info;
                    }
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(updated.status, AgentSessionStatus::Unknown);
        let current = manager.sessions().get_snapshot(&asid).await.unwrap();
        assert!(current.permissions.is_empty());
        assert!(current.forms.is_empty());
        assert_eq!(current.timeline, first.timeline);
        assert!(fixture.requests.try_recv().is_err());
        assert!(herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|r| r["method"] == "agent.list"));
    }

    #[tokio::test]
    async fn native_thread_unload_clears_runtime_ownership_without_deleting_history() {
        for method in ["thread/closed", "thread/status/changed"] {
            let mut fixture = Fixture::new(|r| match r["method"].as_str().unwrap() {
                "thread/start" => json!({"thread":golden()["thread"]}),
                _ => panic!("unload must not issue a provider mutation"),
            })
            .await;
            fixture.ready().await;
            let (tx, mut events) = broadcast::channel(64);
            let manager = AgentManager::connect_codex_observed(
                fixture.client.clone(),
                fixture.events.take().unwrap(),
                tx,
                Some(crate::agents::terminal_runtime::TerminalRuntime::new(&[])),
            );
            let info = manager
                .sessions()
                .create_session(None, None, None)
                .await
                .unwrap();
            assert_eq!(info.runtime.as_ref().unwrap().source, RuntimeSource::Native);
            fixture.send(json!({"method":method,"params":{"threadId":info.asid.0,"status":{"type":"notLoaded"}}})).await;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let AgentDomainEvent::SessionUpdated { info, .. } =
                        events.recv().await.unwrap()
                    {
                        if info
                            .runtime
                            .as_ref()
                            .is_some_and(|r| r.source == RuntimeSource::Unobserved)
                        {
                            break;
                        }
                    }
                }
            })
            .await
            .unwrap();
            let snapshot = manager.sessions().get_snapshot(&info.asid).await.unwrap();
            assert!(!snapshot.info.deleted);
            assert_eq!(snapshot.info.status, AgentSessionStatus::Unknown);
            assert_eq!(
                snapshot.info.runtime.unwrap().interaction,
                RuntimeInteraction::Native
            );
        }
    }

    #[tokio::test]
    async fn native_ownership_precedes_timeline_when_resume_metadata_was_superseded() {
        let mut fixture = Fixture::new(|_| json!({"thread":golden()["thread"]})).await;
        fixture.ready().await;
        let mut driver = CodexDriver::new(fixture.client.clone());
        let observer = crate::agents::terminal_runtime::TerminalRuntime::new(&[]);
        driver.observe_terminals(Some(observer.clone()));
        let mut info = driver.create_session(None, None, None).await.unwrap();
        let asid = info.asid.clone();
        // Model a retained historical view before its queued resume snapshot is
        // discarded by a newer metadata revision. The real driver already owns it.
        observer
            .observe(&asid.0, false, false)
            .await
            .apply(&mut info);
        let mirror = Arc::new(MemoryMirror::for_agent("codex"));
        mirror.update_session(info).await;
        let (tx, mut events) = broadcast::channel(16);
        let context = Context {
            driver: Arc::new(driver),
            mirror,
            tx,
            stopped: Arc::new(Mutex::new(false)),
            threads: tokio::sync::Mutex::new(HashSet::new()),
        };
        context
            .notification(
                "item/completed",
                &json!({"threadId":asid.0,"turnId":"fixture-turn",
            "item":{"type":"agentMessage","id":"fixture-item","text":"reply"}}),
            )
            .await;
        let AgentDomainEvent::SessionUpdated { info, .. } = events.try_recv().unwrap() else {
            panic!("ownership must be first")
        };
        assert_eq!(info.runtime.unwrap().source, RuntimeSource::Native);
        assert!(matches!(
            events.try_recv().unwrap(),
            AgentDomainEvent::TimelineUpsert { .. }
        ));
    }

    #[tokio::test]
    async fn usage_events_survive_metadata_and_tool_rows_replace_provisional_targets() {
        let mut fixture =
            Fixture::new(|_| panic!("seeded projection must not send commands")).await;
        fixture.ready().await;
        let driver = Arc::new(CodexDriver::new(fixture.client.clone()));
        let mirror = Arc::new(MemoryMirror::for_agent("codex"));
        let (tx, mut events) = broadcast::channel(64);
        let context = Context {
            driver: driver.clone(),
            mirror: mirror.clone(),
            tx,
            stopped: Arc::new(Mutex::new(false)),
            threads: tokio::sync::Mutex::new(HashSet::new()),
        };
        let thread = golden()["thread"].clone();
        let id = thread["id"].as_str().unwrap();
        let asid = AgentSessionId(id.into());
        context.session(&thread, 0).await;
        let total = json!({"totalTokens":110,"inputTokens":100,"cachedInputTokens":40,"cacheWriteInputTokens":60,"outputTokens":10,"reasoningOutputTokens":5});
        context.notification("thread/tokenUsage/updated",&json!({"threadId":id,"turnId":"turn","tokenUsage":{"total":total,"last":total,"modelContextWindow":1000}})).await;
        context.session(&thread, 0).await;
        let info = mirror.session_info(&asid).await.unwrap();
        assert_eq!(info.tokens.unwrap().output, 5);
        assert_eq!(info.context_usage.unwrap().total_tokens, 110);
        let mut published_usage = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event,AgentDomainEvent::SessionUpdated {info,..} if info.context_usage.is_some())
            {
                published_usage = true;
            }
        }
        assert!(published_usage);
        for targets in [json!([]), json!(["child"])] {
            context.notification("item/completed",&json!({"threadId":id,"turnId":"turn","item":{"type":"collabAgentToolCall","id":"spawn","tool":"spawnAgent","status":"completed","receiverThreadIds":targets,"agentsStates":{}}})).await;
        }
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snapshot.timeline.len(), 1);
        let AgentPart::Tool(call) = &snapshot.timeline[0].part else {
            panic!("expected tool")
        };
        assert_eq!(call.child_session_id.as_deref(), Some("child"));
        assert!(call.metadata.as_ref().unwrap().get("status").is_none());
        context.notification("turn/plan/updated",&json!({"threadId":id,"turnId":"turn","explanation":"First plan","plan":[{"step":"Inspect","status":"completed"},{"step":"Fix","status":"inProgress"}]})).await;
        context.notification("turn/plan/updated",&json!({"threadId":id,"turnId":"turn","explanation":null,"plan":[{"step":"Inspect","status":"completed"}]})).await;
        context
            .notification(
                "turn/diff/updated",
                &json!({"threadId":id,"turnId":"turn","diff":"--- a/a.rs\n+++ b/a.rs\n-old\n+new"}),
            )
            .await;
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snapshot.timeline.len(), 3);
        assert!(snapshot.timeline.iter().any(
            |row| matches!(&row.part, AgentPart::Todo{items} if items.len() == 1 && items[0].done)
        ));
        assert!(snapshot.timeline.iter().any(|row| matches!(&row.part, AgentPart::Diff{file,diff} if file.is_empty() && diff.contains("+new"))));
    }

    async fn next_session_update(
        events: &mut broadcast::Receiver<AgentDomainEvent>,
    ) -> AgentSessionInfo {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let AgentDomainEvent::SessionUpdated { info, .. } = events.recv().await.unwrap()
                {
                    return *info;
                }
            }
        })
        .await
        .unwrap()
    }

    struct ModelPageRpc {
        client: Arc<CodexClient>,
        events: Option<mpsc::Receiver<CodexEvent>>,
        requests: mpsc::Receiver<Value>,
        writer: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl ModelPageRpc {
        async fn new() -> Self {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let (client_io, server_io) = tokio::io::duplex(8192);
            let (read, write) = tokio::io::split(client_io);
            let (client, events) = CodexClient::from_io(read, write);
            let (read, write) = tokio::io::split(server_io);
            let writer = Arc::new(tokio::sync::Mutex::new(write));
            let replies = writer.clone();
            let (tx, requests) = mpsc::channel(32);
            let task = tokio::spawn(async move {
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let result = match request["method"].as_str() {
                        Some("initialize") => Some(golden()["initialize"].clone()),
                        Some("account/read") => Some(golden()["account"].clone()),
                        Some("initialized") => continue,
                        _ => None,
                    };
                    if let Some(result) = result {
                        Self::write(&replies, json!({"id":request["id"],"result":result})).await;
                    } else if tx.send(request).await.is_err() {
                        break;
                    }
                }
            });
            client.initialize().await.unwrap();
            Self {
                client,
                events: Some(events),
                requests,
                writer,
                task,
            }
        }

        async fn write(
            writer: &tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
            value: Value,
        ) {
            use tokio::io::AsyncWriteExt;
            let mut bytes = serde_json::to_vec(&value).unwrap();
            bytes.push(b'\n');
            writer.lock().await.write_all(&bytes).await.unwrap();
        }

        async fn send(&self, value: Value) {
            Self::write(&self.writer, value).await;
        }

        async fn next(&mut self, method: &str) -> Value {
            let request = tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request["method"], method);
            request
        }
    }

    impl Drop for ModelPageRpc {
        fn drop(&mut self) {
            self.client.stop();
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn codex_model_page_slow_old_refresh_does_not_block_callbacks_or_replace_new_turn() {
        let mut fixture = ModelPageRpc::new().await;
        let (tx, mut events) = broadcast::channel(32);
        let manager = Arc::new(AgentManager::connect_codex(
            fixture.client.clone(),
            fixture.events.take().unwrap(),
            tx,
        ));
        let thread = golden()["thread"].clone();
        let id = thread["id"].as_str().unwrap().to_owned();
        fixture
            .send(json!({"method":"thread/started","params":{"thread":thread}}))
            .await;
        next_session_update(&mut events).await;
        let mut reads = vec![];
        for turn in ["synthetic-first", "synthetic-second"] {
            let manager = manager.clone();
            let asid = AgentSessionId(id.clone());
            let prompt = tokio::spawn(async move {
                manager
                    .prompts()
                    .send_prompt(&asid, "synthetic prompt", &[], None)
                    .await
            });
            // Seeded stream metadata is not explicit loaded ownership.
            if turn == "synthetic-first" {
                let read = fixture.next("thread/read").await;
                fixture
                    .send(json!({"id":read["id"],"result":{"thread":thread}}))
                    .await;
                let resume = fixture.next("thread/resume").await;
                fixture
                    .send(json!({"id":resume["id"],"result":{"thread":thread}}))
                    .await;
            }
            let start = fixture.next("turn/start").await;
            fixture
                .send(json!({"id":start["id"],"result":{"turn":{"id":turn,"status":"inProgress"}}}))
                .await;
            prompt.await.unwrap().unwrap();
            reads.push(fixture.next("thread/read").await);
            if turn == "synthetic-first" {
                fixture.send(json!({"id":77,"method":"item/fileChange/requestApproval","params":{"threadId":id,"turnId":turn,"itemId":"synthetic-tool"}})).await;
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if matches!(
                            events.recv().await.unwrap(),
                            AgentDomainEvent::PermissionPending { .. }
                        ) {
                            break;
                        }
                    }
                })
                .await
                .expect("metadata RPC must not hold the event loop");
                fixture.send(json!({"method":"turn/completed","params":{"threadId":id,"turn":{"id":turn,"status":"completed","items":[]}}})).await;
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if matches!(
                            events.recv().await.unwrap(),
                            AgentDomainEvent::PermissionResolved { .. }
                        ) {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
        }
        let mut actual = thread.clone();
        actual["model"] = json!("synthetic-second-normalized");
        actual["reasoningEffort"] = json!("low");
        actual["status"] = json!({"type":"active"});
        fixture.send(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":"synthetic-second"}}})).await;
        fixture.send(json!({"method":"turn/completed","params":{"threadId":id,"turn":{"id":"synthetic-second","status":"completed","items":[]}}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    AgentDomainEvent::StatusChanged {
                        status: AgentSessionStatus::Idle,
                        ..
                    }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        fixture
            .send(json!({"id":reads[1]["id"],"result":{"thread":actual}}))
            .await;
        let updated = next_session_update(&mut events).await;
        assert_eq!(
            updated.model.unwrap().model_id,
            "synthetic-second-normalized"
        );
        assert_eq!(
            updated.status,
            AgentSessionStatus::Idle,
            "slow read cannot revert a completed turn's status"
        );
        fixture
            .send(json!({"id":reads[0]["id"],"result":{"thread":thread}}))
            .await;
        // A read-only rename acts as an ordered wire/event barrier after the old
        // response, without introducing time-dependent sleeps.
        fixture
            .send(json!({"method":"thread/name/updated","params":{"threadId":id,"name":"barrier"}}))
            .await;
        let barrier = fixture.next("thread/read").await;
        fixture
            .send(json!({"id":barrier["id"],"result":{"thread":actual}}))
            .await;
        assert_eq!(
            next_session_update(&mut events)
                .await
                .model
                .unwrap()
                .model_id,
            "synthetic-second-normalized"
        );
        assert_eq!(
            manager
                .sessions()
                .get_snapshot(&AgentSessionId(id.clone()))
                .await
                .unwrap()
                .info
                .model
                .unwrap()
                .model_id,
            "synthetic-second-normalized"
        );
        fixture.send(json!({"method":"thread/name/updated","params":{"threadId":id,"name":"synthetic pending on shutdown"}})).await;
        fixture.next("thread/read").await;
        manager.shutdown();
        drop(manager);
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&fixture.client) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pending read and old pump release their client after shutdown");
        assert!(
            events.try_recv().is_err(),
            "shutdown fences late metadata events"
        );
    }

    #[tokio::test]
    async fn codex_model_page_accepted_turn_updates_service_snapshot_with_normalized_source() {
        let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = accepted.clone();
        let mut fixture = Fixture::new(move |r| {
            let mut thread = golden()["thread"].clone();
            thread["modelProvider"] = json!("synthetic-oss");
            thread["reasoningEffort"] = json!("medium");
            if state.load(std::sync::atomic::Ordering::Acquire) {
                thread["model"] = json!("synthetic-normalized-model");
                thread["reasoningEffort"] = json!("low");
                thread["status"] = json!({"type":"active"});
            }
            match r["method"].as_str().unwrap() {
                "config/read" => json!({"config":{"model_provider":"synthetic-oss"}}),
                "model/list" => golden()["modelList"].clone(),
                "thread/start" => json!({"thread":thread}),
                "thread/read" => {
                    assert_eq!(r["params"]["includeTurns"], false);
                    json!({"thread":thread})
                }
                "turn/start" => {
                    assert_eq!(r["params"]["model"], "fixture-model");
                    assert_eq!(r["params"]["effort"], "high");
                    assert!(r["params"].get("modelProvider").is_none());
                    state.store(true, std::sync::atomic::Ordering::Release);
                    json!({"turn":{"id":"synthetic-turn","status":"inProgress"}})
                }
                _ => panic!("unexpected model fixture RPC"),
            }
        })
        .await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(32);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        let initial = manager
            .sessions()
            .create_session(None, None, None)
            .await
            .unwrap();
        next_session_update(&mut events).await;
        let asid = initial.asid;
        manager.view_session(&asid.0, u64::MAX).await.unwrap();
        next_session_update(&mut events).await;
        let choice = ModelRef {
            provider_id: "synthetic-oss".into(),
            model_id: "fixture-model".into(),
            variant: Some("high".into()),
        };
        manager
            .sessions()
            .switch_model(&asid, &choice)
            .await
            .unwrap();
        assert_eq!(
            manager
                .sessions()
                .get_snapshot(&asid)
                .await
                .unwrap()
                .info
                .model
                .unwrap()
                .variant
                .as_deref(),
            Some("medium")
        );
        assert!(
            events.try_recv().is_err(),
            "no requested-selection snapshot before acceptance"
        );
        manager
            .prompts()
            .send_prompt(&asid, "synthetic prompt", &[], None)
            .await
            .unwrap();
        let actual = next_session_update(&mut events).await;
        assert_eq!(
            actual.model.as_ref().unwrap(),
            &ModelRef {
                provider_id: "synthetic-oss".into(),
                model_id: "synthetic-normalized-model".into(),
                variant: Some("low".into())
            }
        );
        assert_ne!(actual.model.as_ref(), Some(&choice));
        assert_eq!(actual.agent_id, "codex");
        assert_eq!(actual.time_viewed, Some(u64::MAX));
        assert_eq!(
            manager
                .sessions()
                .get_snapshot(&asid)
                .await
                .unwrap()
                .info
                .model,
            actual.model
        );
        let requests: Vec<_> = std::iter::from_fn(|| fixture.requests.try_recv().ok()).collect();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "turn/start")
                .count(),
            1
        );
        assert!(!requests.iter().any(|r| r["method"] == "thread/resume"));
        manager.shutdown();
    }

    #[tokio::test]
    async fn codex_model_page_metadata_failure_keeps_acceptance_and_resyncs_without_retry() {
        let recovered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovery = recovered.clone();
        let mut fixture = Fixture::new(move |r| match r["method"].as_str().unwrap() {
            "thread/start" => json!({"thread":golden()["thread"]}),
            "turn/start" => json!({"turn":{"id":"synthetic-turn","status":"inProgress"}}),
            // A malformed/foreign response is a failed authoritative refresh.
            "thread/read" => {
                if !recovery.load(std::sync::atomic::Ordering::Acquire) {
                    return json!({"thread":{"id":"synthetic-wrong"}});
                }
                let mut thread = golden()["thread"].clone();
                thread["model"] = json!("synthetic-recovered");
                thread["reasoningEffort"] = json!("low");
                json!({"thread":thread})
            }
            _ => panic!("unexpected metadata-failure fixture RPC"),
        })
        .await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(32);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        let initial = manager
            .sessions()
            .create_session(None, None, None)
            .await
            .unwrap();
        next_session_update(&mut events).await;
        manager
            .prompts()
            .send_prompt(&initial.asid, "synthetic accepted prompt", &[], None)
            .await
            .unwrap();
        assert!(next_session_update(&mut events).await.model.is_none());
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, AgentDomainEvent::Resync {asid,reason} if asid == initial.asid && reason == "codex_metadata_refresh_failed_resync_required")
        );
        assert!(manager
            .sessions()
            .get_snapshot(&initial.asid)
            .await
            .unwrap()
            .info
            .model
            .is_none());
        assert!(manager
            .prompts()
            .send_prompt(&initial.asid, "must not automatically replay", &[], None)
            .await
            .is_err());
        let requests: Vec<_> = std::iter::from_fn(|| fixture.requests.try_recv().ok()).collect();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "turn/start")
                .count(),
            1
        );
        recovered.store(true, std::sync::atomic::Ordering::Release);
        fixture.send(json!({"method":"thread/name/updated","params":{"threadId":initial.asid.0,"name":"synthetic recovery"}})).await;
        assert_eq!(
            next_session_update(&mut events)
                .await
                .model
                .unwrap()
                .model_id,
            "synthetic-recovered"
        );
        assert_eq!(
            manager
                .sessions()
                .get_snapshot(&initial.asid)
                .await
                .unwrap()
                .info
                .model
                .unwrap()
                .variant
                .as_deref(),
            Some("low")
        );
        assert_eq!(
            fixture.next().await["method"],
            "thread/read",
            "recovery reads metadata, never resubmits a prompt"
        );
        assert!(fixture.requests.try_recv().is_err());
        manager.shutdown();
    }

    #[tokio::test]
    async fn codex_model_page_neutral_route_is_forward_only_for_asc_and_desc() {
        use tower::ServiceExt;
        for order in ["asc", "desc"] {
            let mut fixture = Fixture::new(move |r| {
                assert_eq!(r["method"], "thread/list");
                assert_eq!(r["params"]["sortDirection"], order);
                assert_eq!(r["params"]["sortKey"], "updated_at");
                assert_eq!(r["params"]["useStateDbOnly"], true);
                assert_eq!(r["params"]["cwd"], "/synthetic/workspace");
                assert_eq!(r["params"]["searchTerm"], "Synthetic");
                assert_eq!(r["params"]["limit"], 2);
                let offset = r["params"]["cursor"].as_str().map(|cursor| cursor.strip_prefix("model_page:").unwrap().parse::<usize>().unwrap()).unwrap_or(0);
                let end = (offset + 2).min(5);
                let rows: Vec<_> = (offset..end).map(|position| {
                    let index = if order == "asc" { position } else { 4 - position };
                    let mut thread = golden()["thread"].clone();
                    thread["id"] = json!(format!("synthetic-{index}"));
                    thread["updatedAt"] = json!(100 + index);
                    if index < 2 { thread["parentThreadId"] = json!("synthetic-parent"); }
                    thread
                }).collect();
                json!({"data":rows,"nextCursor":if end < 5 {json!(format!("model_page:{end}"))} else {Value::Null},"backwardsCursor":"native-requires-flipped-order"})
            }).await;
            fixture.ready().await;
            let (tx, _) = broadcast::channel(32);
            let manager = Arc::new(AgentManager::connect_codex(
                fixture.client.clone(),
                fixture.events.take().unwrap(),
                tx,
            ));
            // This constructor disables all real service-agent supervision.
            let state = crate::test_support::test_state(
                "admin",
                vec![crate::test_support::test_device("phone", "device-token")],
            );
            state.agent_runtime.attach_for_test(manager.clone()).await;
            let router = crate::agents::routes::mount(axum::Router::new()).with_state(state);
            let mut cursor = None;
            let mut ids = vec![];
            for page in 0..3 {
                let uri = format!("/api/agent-sessions/page?agent_id=codex&order={order}&limit=2&roots=true&directory=%2Fsynthetic%2Fworkspace&search=Synthetic{}", cursor.as_ref().map(|cursor| format!("&cursor={cursor}")).unwrap_or_default());
                let response = router
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri(uri)
                            .header("authorization", "Bearer device-token")
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), axum::http::StatusCode::OK);
                let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&bytes).unwrap();
                assert!(body["data"]["cursor"]["previous"].is_null());
                let rows = body["data"]["data"].as_array().unwrap();
                assert!(rows.len() <= 2);
                for row in rows {
                    assert_eq!(row["agent_id"], "codex");
                    ids.push(row["asid"].as_str().unwrap().to_owned());
                }
                cursor = body["data"]["cursor"]["next"].as_str().map(str::to_owned);
                assert_eq!(cursor.is_some(), page < 2);
                let wire = fixture.next().await;
                assert_eq!(
                    wire["params"]["cursor"],
                    if page == 0 {
                        Value::Null
                    } else {
                        json!(format!("model_page:{}", page * 2))
                    }
                );
            }
            assert_eq!(
                ids,
                if order == "asc" {
                    vec!["synthetic-2", "synthetic-3", "synthetic-4"]
                } else {
                    vec!["synthetic-4", "synthetic-3", "synthetic-2"]
                }
            );
            assert!(fixture.requests.try_recv().is_err());
            manager.shutdown();
        }
    }

    #[tokio::test]
    async fn codex_manager_drop_and_replacement_release_pump_and_fence_old_events() {
        let mut old = Fixture::new(|_| panic!("seeded old thread needs no RPC")).await;
        old.ready().await;
        let (tx, mut events) = broadcast::channel(32);
        let manager =
            AgentManager::connect_codex(old.client.clone(), old.events.take().unwrap(), tx.clone());
        let mut thread = golden()["thread"].clone();
        thread["id"] = json!("synthetic-old");
        old.send(json!({"method":"thread/started","params":{"thread":thread}}))
            .await;
        assert!(
            matches!(events.recv().await.unwrap(),AgentDomainEvent::SessionUpdated {asid,..} if asid.0=="synthetic-old")
        );
        drop(manager);
        assert!(!old.client.is_ready());
        tokio::time::timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&old.client) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // No external manager means no strong event-pump ownership remains.
        let mut replacement = Fixture::new(|_| panic!("seeded replacement needs no RPC")).await;
        replacement.ready().await;
        let manager = AgentManager::connect_codex(
            replacement.client.clone(),
            replacement.events.take().unwrap(),
            tx,
        );
        let mut thread = golden()["thread"].clone();
        thread["id"] = json!("synthetic-new");
        replacement
            .send(json!({"method":"thread/started","params":{"thread":thread}}))
            .await;
        assert!(
            matches!(events.recv().await.unwrap(),AgentDomainEvent::SessionUpdated {asid,..} if asid.0=="synthetic-new")
        );
        manager.shutdown();
        assert!(!replacement.client.is_ready());
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "no late disconnected/resync events from either stopped generation"
        );
    }

    #[tokio::test]
    async fn codex_rename_rehydrates_metadata_without_losing_owner_or_read_marker() {
        let mut fixture = Fixture::new(|r| {
            assert_eq!(r["method"], "thread/read");
            assert_eq!(r["params"]["includeTurns"], false);
            let mut thread = golden()["thread"].clone();
            thread["id"] = json!("synthetic-a");
            thread["name"] = json!("Synthetic renamed");
            json!({"thread":thread})
        })
        .await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(32);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        let mut thread = golden()["thread"].clone();
        thread["id"] = json!("synthetic-a");
        fixture
            .send(json!({"method":"thread/started","params":{"thread":thread}}))
            .await;
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        let asid = AgentSessionId("synthetic-a".into());
        manager.mirror.mark_viewed(&asid, u64::MAX).await.unwrap();
        fixture.send(json!({"method":"thread/name/updated","params":{"threadId":"synthetic-a","name":"Synthetic renamed"}})).await;
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event,AgentDomainEvent::SessionUpdated {info,..} if info.agent_id == "codex" && info.title == "Synthetic renamed" && info.time_viewed == Some(u64::MAX))
        );
        manager.shutdown();
    }

    #[tokio::test]
    async fn codex_two_threads_interleave_text_reasoning_tools_without_duplicate_final_rows() {
        let mut fixture =
            Fixture::new(|_| panic!("seeded sessions need no history/resume RPC")).await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(128);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        for id in ["synthetic-a", "synthetic-b"] {
            let mut thread = golden()["thread"].clone();
            thread["id"] = json!(id);
            fixture
                .send(json!({"method":"thread/started","params":{"thread":thread}}))
                .await;
        }
        for id in ["synthetic-a", "synthetic-b"] {
            fixture.send(json!({"method":"turn/started","params":{"threadId":id,"turn":{"id":"synthetic-turn","status":"inProgress"}}})).await;
        }
        for (thread, delta) in [
            ("synthetic-a", "a"),
            ("synthetic-b", "b"),
            ("synthetic-a", "-full"),
            ("synthetic-b", "-full"),
        ] {
            fixture.send(json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":"synthetic-turn","itemId":"synthetic-message","delta":delta}})).await;
        }
        fixture.send(json!({"method":"item/reasoning/summaryTextDelta","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-reasoning","summaryIndex":0,"delta":"summary"}})).await;
        fixture.send(json!({"method":"item/reasoning/textDelta","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-reasoning","contentIndex":0,"delta":"raw"}})).await;
        fixture.send(json!({"method":"item/completed","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","item":{"type":"reasoning","id":"synthetic-reasoning","summary":["summary"],"content":["raw"]}}})).await;
        for (thread, text) in [("synthetic-a", "a-full"), ("synthetic-b", "b-full")] {
            fixture.send(json!({"method":"item/completed","params":{"threadId":thread,"turnId":"synthetic-turn","item":{"type":"agentMessage","id":"synthetic-message","text":text}}})).await;
        }
        for (method, status, output) in [
            ("item/started", "inProgress", Value::Null),
            ("item/completed", "completed", json!("synthetic output")),
        ] {
            fixture.send(json!({"method":method,"params":{"threadId":"synthetic-b","turnId":"synthetic-turn","item":{"type":"commandExecution","id":"synthetic-tool","command":"echo synthetic","cwd":"/synthetic/workspace","status":status,"aggregatedOutput":output}}})).await;
        }
        fixture.send(json!({"method":"turn/completed","params":{"threadId":"synthetic-a","turn":{"id":"synthetic-turn","status":"completed","items":[]}}})).await;
        fixture.send(json!({"method":"turn/completed","params":{"threadId":"synthetic-b","turn":{"id":"synthetic-turn","status":"interrupted","items":[]}}})).await;
        tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                if matches!(events.recv().await.unwrap(),AgentDomainEvent::StatusChanged {asid,status:AgentSessionStatus::Interrupted,..} if asid.0 == "synthetic-b") {break;}
            }
        }).await.unwrap();
        let a = manager
            .mirror
            .get_snapshot(&AgentSessionId("synthetic-a".into()))
            .await
            .unwrap();
        let b = manager
            .mirror
            .get_snapshot(&AgentSessionId("synthetic-b".into()))
            .await
            .unwrap();
        assert_eq!(
            a.timeline
                .iter()
                .filter(|r| matches!(&r.part,AgentPart::Text{text} if text == "a-full"))
                .count(),
            1
        );
        assert_eq!(
            b.timeline
                .iter()
                .filter(|r| matches!(&r.part,AgentPart::Text{text} if text == "b-full"))
                .count(),
            1
        );
        assert_eq!(a.timeline.len(), 3);
        assert_eq!(b.timeline.len(), 2);
        assert_eq!(a.info.status, AgentSessionStatus::Idle);
        assert_eq!(b.info.status, AgentSessionStatus::Interrupted);
        assert!(
            matches!(&b.timeline.iter().find(|r|matches!(r.part,AgentPart::Tool(_))).unwrap().part,AgentPart::Tool(t) if t.state == ToolCallStatus::Completed && t.output == Some(json!("synthetic output")))
        );
        let expected = mapper::item(
            "synthetic-b",
            "synthetic-turn",
            &json!({"id":"synthetic-message","type":"agentMessage","text":"b-full"}),
            0,
        );
        assert!(b.timeline.iter().any(|r| r.id == expected[0].id));
        manager.shutdown();
    }

    #[tokio::test]
    async fn codex_manager_expired_requests_resolve_and_disconnect_becomes_uncertain_resync() {
        let mut fixture = Fixture::new(|_| json!({})).await;
        fixture.ready().await;
        let (tx, mut events) = broadcast::channel(128);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        let mut thread = golden()["thread"].clone();
        thread["id"] = json!("synthetic-a");
        fixture
            .send(json!({"method":"thread/started","params":{"thread":thread}}))
            .await;
        fixture.send(json!({"method":"turn/started","params":{"threadId":"synthetic-a","turn":{"id":"synthetic-turn"}}})).await;
        fixture.send(json!({"id":10,"method":"item/fileChange/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool"}})).await;
        let request_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let AgentDomainEvent::PermissionPending { request, .. } =
                    events.recv().await.unwrap()
                {
                    break request.id;
                }
            }
        })
        .await
        .unwrap();
        fixture.send(json!({"method":"serverRequest/resolved","params":{"threadId":"synthetic-a","requestId":10}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    AgentDomainEvent::PermissionResolved { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(manager
            .agent
            .reply_permission("synthetic-a", &request_id, PermissionDecision::Allow, None)
            .await
            .is_err());
        fixture.send(json!({"id":11,"method":"item/fileChange/requestApproval","params":{"threadId":"synthetic-a","turnId":"synthetic-turn","itemId":"synthetic-tool"}})).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await.unwrap(),
                    AgentDomainEvent::PermissionPending { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        fixture.client.stop();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let event = events.recv().await.unwrap();
                assert!(
                    !matches!(
                        event,
                        AgentDomainEvent::PermissionResolved { .. }
                            | AgentDomainEvent::FormResolved { .. }
                    ),
                    "transport loss is expiry/resync, not successful approval resolution"
                );
                if matches!(event, AgentDomainEvent::Resync { .. }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let snapshot = manager
            .mirror
            .get_snapshot(&AgentSessionId("synthetic-a".into()))
            .await
            .unwrap();
        assert_eq!(snapshot.info.status, AgentSessionStatus::Unknown);
        assert!(snapshot.permissions.is_empty());
    }

    #[tokio::test]
    async fn codex_complex_elicitation_is_rejected_not_an_ordinary_form_or_approval() {
        let mut fixture = Fixture::new(|_| json!({})).await;
        fixture.ready().await;
        let (tx, _events) = broadcast::channel(32);
        let manager =
            AgentManager::connect_codex(fixture.client.clone(), fixture.events.take().unwrap(), tx);
        fixture.send(json!({"id":"synthetic-url","method":"mcpServer/elicitation/request","params":{"threadId":"synthetic-a","mode":"url","url":"https://synthetic.invalid/auth"}})).await;
        let response = fixture.next().await;
        assert_eq!(response["id"], "synthetic-url");
        assert_eq!(response["error"]["code"], -32601);
        assert!(manager
            .agent
            .get_pending_forms("synthetic-a")
            .await
            .unwrap()
            .is_empty());
        assert!(manager
            .agent
            .get_pending_permissions("synthetic-a")
            .await
            .unwrap()
            .is_empty());
        manager.shutdown();
    }
}
