//! DeepSeek Harness event processing: the counterpart of
//! `AgentManager::handle_raw_event` for the frames
//! `DeepseekStreamListener` publishes. Each frame updates the mirror and
//! emits the same `AgentDomainEvent`s the OpenCode path does, so the app
//! sees one vocabulary.
//!
//! The wire facts this relies on are documented at the top of
//! `adapters/deepseek/stream.rs`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex};

use crate::agents::adapters::deepseek::stream::{
    DeepseekInteractions, DeepseekStreamEvent, DeepseekStreamListener, EventsFrame,
    PendingApproval, PendingQuestion,
};
use crate::agents::adapters::deepseek::{mapper, DeepseekDriver};
use crate::agents::adapters::memory_mirror::{MemoryMirror, SessionPatch, ToolPatch};
use crate::agents::domain::{
    reasoning_item_id, text_item_id, AgentDomainEvent, AgentErrorInfo, AgentPart, AgentSessionId,
    AgentSessionStatus, CompactionStatus, SessionQuery, TimelineItem, ToolCallStatus,
};
use crate::agents::ports::mirror::SessionMirrorPort;

/// How long after a new `$events` generation opens before a pending prompt
/// the host did not re-deliver is taken to have been settled while the
/// gateway was away. Re-delivery is queued before `ready` is even sent, so
/// this only has to cover socket latency.
const PENDING_CONFIRM_GRACE: Duration = Duration::from_secs(2);
/// Tool-call owners remembered per manager, so a `tool/result` can find its
/// card. Bounded because a long-lived gateway sees many.
const MAX_TRACKED_TOOL_CALLS: usize = 1024;

/// The assistant attempt currently streaming in one session.
#[derive(Debug, Clone)]
struct ActiveAttempt {
    attempt_id: String,
    /// The provisional message id its rows stream under.
    message_id: String,
    /// Every row created for it, to drop once the message commits.
    row_ids: Vec<String>,
}

pub(crate) struct DeepseekEventContext {
    pub driver: Arc<DeepseekDriver>,
    pub mirror: Arc<MemoryMirror>,
    pub tx: broadcast::Sender<AgentDomainEvent>,
    pub listener: Arc<DeepseekStreamListener>,
    pub interactions: Arc<DeepseekInteractions>,
    attempts: Mutex<HashMap<String, ActiveAttempt>>,
    /// `(session id, call id)` -> the message id the call's card lives under.
    tool_owners: Mutex<HashMap<(String, String), String>>,
}

impl DeepseekEventContext {
    pub(crate) fn new(
        driver: Arc<DeepseekDriver>,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
        listener: Arc<DeepseekStreamListener>,
        interactions: Arc<DeepseekInteractions>,
    ) -> Self {
        Self {
            driver,
            mirror,
            tx,
            listener,
            interactions,
            attempts: Mutex::new(HashMap::new()),
            tool_owners: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn emit(&self, mut event: AgentDomainEvent) {
        self.mirror.stamp_event(&mut event);
        // A send with no subscribers is not a failure: nobody is watching.
        let _ = self.tx.send(event);
    }

    async fn set_status(
        &self,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
        error: Option<AgentErrorInfo>,
    ) {
        if let Some(seq) = self.mirror.update_status(asid, status, error.clone()).await {
            self.emit(AgentDomainEvent::StatusChanged {
                asid: asid.clone(),
                status,
                error,
                seq,
            });
        }
    }

    async fn patch_session(&self, asid: &AgentSessionId, patch: SessionPatch) {
        if let Some((seq, info)) = self.mirror.merge_session_fields(asid, patch).await {
            self.emit(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(info),
                seq,
            });
        }
    }

    async fn upsert_items(&self, asid: &AgentSessionId, items: Vec<TimelineItem>) {
        if items.is_empty() {
            return;
        }
        self.remember_tool_owners(asid, &items).await;
        let seq = self.mirror.upsert_timeline_items(asid, items.clone()).await;
        self.emit(AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items,
            seq,
        });
    }

    async fn upsert_tool(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
        call_id: &str,
        patch: ToolPatch,
    ) -> Option<String> {
        match self
            .mirror
            .upsert_tool_call(asid, message_id, call_id, patch)
            .await
        {
            Some((seq, item)) => {
                let id = item.id.clone();
                self.emit(AgentDomainEvent::TimelineUpsert {
                    asid: asid.clone(),
                    items: vec![item],
                    seq,
                });
                Some(id)
            }
            None => {
                tracing::debug!(asid = %asid.0, call_id, "tool frame dropped: not a tool row");
                None
            }
        }
    }

    async fn remember_tool_owners(&self, asid: &AgentSessionId, items: &[TimelineItem]) {
        let mut owners = self.tool_owners.lock().await;
        for item in items {
            if let AgentPart::Tool(ref call) = item.part {
                if owners.len() >= MAX_TRACKED_TOOL_CALLS {
                    if let Some(key) = owners.keys().next().cloned() {
                        owners.remove(&key);
                    }
                }
                owners.insert((asid.0.clone(), call.id.clone()), item.message_id.clone());
            }
        }
    }

    /// The message a call's card lives under: remembered from the rows that
    /// created it, else found in the mirror.
    async fn tool_owner(&self, asid: &AgentSessionId, call_id: &str) -> Option<String> {
        if let Some(owner) = self
            .tool_owners
            .lock()
            .await
            .get(&(asid.0.clone(), call_id.to_string()))
            .cloned()
        {
            return Some(owner);
        }
        let snapshot = self.mirror.get_snapshot(asid).await?;
        snapshot.timeline.iter().find_map(|item| match &item.part {
            AgentPart::Tool(call) if call.id == call_id => Some(item.message_id.clone()),
            _ => None,
        })
    }

    /// Drop the provisional rows of the attempt streaming in `asid`, if any.
    async fn finish_attempt(&self, asid: &AgentSessionId) {
        let Some(attempt) = self.attempts.lock().await.remove(&asid.0) else {
            return;
        };
        if let Some((seq, ids)) = self
            .mirror
            .remove_timeline_items(asid, &attempt.row_ids)
            .await
        {
            self.emit(AgentDomainEvent::TimelineRemoved {
                asid: asid.clone(),
                ids,
                seq,
            });
        }
    }

    async fn resolve_permission(&self, asid: &AgentSessionId, request_id: &str) {
        let seq = self.mirror.resolve_permission(asid, request_id).await;
        self.emit(AgentDomainEvent::PermissionResolved {
            asid: asid.clone(),
            request_id: request_id.to_string(),
            seq,
        });
    }

    async fn resolve_form(&self, asid: &AgentSessionId, form_id: &str) {
        let seq = self.mirror.resolve_form(asid, form_id).await;
        self.emit(AgentDomainEvent::FormResolved {
            asid: asid.clone(),
            form_id: form_id.to_string(),
            seq,
        });
    }
}

/// Whether a `SessionSummary` names a session with a live agent, which is
/// the only kind worth (and safe) following.
fn summary_is_live(summary: &Value) -> bool {
    summary
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || summary
            .get("agentAvailable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

pub(crate) async fn handle_deepseek_event(event: DeepseekStreamEvent, ctx: &DeepseekEventContext) {
    match event {
        DeepseekStreamEvent::Connected => seed_followed_sessions(ctx).await,
        DeepseekStreamEvent::Disconnected => {
            ctx.interactions.clear_client();
            ctx.attempts.lock().await.clear();
        }
        DeepseekStreamEvent::Events(frame) => handle_events_frame(frame, ctx).await,
        DeepseekStreamEvent::Follow { session_id, frame } => {
            handle_follow_frame(&AgentSessionId(session_id), &frame, ctx).await
        }
        DeepseekStreamEvent::FollowEnded { session_id, error } => {
            if let Some(error) = error {
                tracing::debug!(session_id, ?error, "deepseek follow ended with an error");
            }
            ctx.attempts.lock().await.remove(&session_id);
        }
    }
}

/// On every (re)connect, follow the sessions the host currently has live.
/// The stream does not replay `api-session/added` for them.
async fn seed_followed_sessions(ctx: &DeepseekEventContext) {
    let summaries = match ctx
        .driver
        .client()
        .list_sessions(&SessionQuery::default())
        .await
    {
        Ok(summaries) => summaries,
        Err(err) => {
            tracing::warn!(%err, "deepseek session list for follow seeding failed");
            return;
        }
    };
    for summary in summaries.iter().filter(|s| summary_is_live(s)) {
        if let Some(info) = mapper::map_session(summary) {
            let asid = info.asid.clone();
            let seq = ctx.mirror.update_session(info.clone()).await;
            ctx.emit(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(info),
                seq,
            });
            ctx.listener.follow(&asid.0);
        }
    }
}

async fn handle_events_frame(frame: EventsFrame, ctx: &DeepseekEventContext) {
    match frame {
        EventsFrame::Ready { client_id } => {
            let generation = ctx.interactions.begin_generation(client_id);
            // Whatever the host still holds is re-delivered right behind
            // `ready`; what it does not re-deliver was settled while we were
            // away and must not stay pending on the phone.
            let interactions = ctx.interactions.clone();
            let mirror = ctx.mirror.clone();
            let tx = ctx.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PENDING_CONFIRM_GRACE).await;
                let (approvals, questions) = interactions.expire_unconfirmed(generation);
                for PendingApproval { asid, request } in approvals {
                    let seq = mirror.resolve_permission(&asid, &request.id).await;
                    let _ = tx.send(AgentDomainEvent::PermissionResolved {
                        asid,
                        request_id: request.id,
                        seq,
                    });
                }
                for PendingQuestion { asid, request, .. } in questions {
                    let seq = mirror.resolve_form(&asid, &request.id).await;
                    let _ = tx.send(AgentDomainEvent::FormResolved {
                        asid,
                        form_id: request.id,
                        seq,
                    });
                }
            });
        }
        EventsFrame::Emit { event, args } => handle_emit(&event, &args, ctx).await,
        EventsFrame::Waterfall {
            event,
            event_id,
            agent_id,
            request,
        } => {
            let asid = AgentSessionId(agent_id.clone());
            match event.as_str() {
                "approval/request" => {
                    let permission = mapper::map_approval_request(&event_id, &agent_id, &request);
                    ctx.interactions.add_approval(
                        &event_id,
                        PendingApproval {
                            asid: asid.clone(),
                            request: permission.clone(),
                        },
                    );
                    let seq = ctx.mirror.add_permission(permission.clone()).await;
                    ctx.emit(AgentDomainEvent::PermissionPending {
                        asid,
                        request: permission,
                        seq,
                    });
                }
                "user-questions/request" => {
                    let Some(form) = mapper::map_question_request(&event_id, &agent_id, &request)
                    else {
                        tracing::debug!(event_id, "user question with no questions dropped");
                        return;
                    };
                    ctx.interactions.add_question(
                        &event_id,
                        PendingQuestion {
                            asid: asid.clone(),
                            request: form.clone(),
                            questions: request,
                        },
                    );
                    let seq = ctx.mirror.add_form(form.clone()).await;
                    ctx.emit(AgentDomainEvent::FormPending {
                        asid,
                        request: form,
                        seq,
                    });
                }
                other => {
                    tracing::trace!(event = other, "deepseek waterfall not mapped");
                }
            }
        }
        EventsFrame::Cancel { event_id } => {
            let (approval, question) = ctx.interactions.remove(&event_id);
            if let Some(approval) = approval {
                ctx.resolve_permission(&approval.asid, &event_id).await;
            }
            if let Some(question) = question {
                ctx.resolve_form(&question.asid, &event_id).await;
            }
        }
    }
}

/// The `api-session/*` emits: the session-list stream.
async fn handle_emit(event: &str, args: &[Value], ctx: &DeepseekEventContext) {
    let session_arg = |index: usize| {
        args.get(index)
            .and_then(Value::as_str)
            .map(|s| AgentSessionId(s.to_string()))
    };
    match event {
        "api-session/added" => {
            let Some(summary) = args.first() else {
                return;
            };
            let Some(info) = mapper::map_session(summary) else {
                return;
            };
            let asid = info.asid.clone();
            let seq = ctx.mirror.update_session(info.clone()).await;
            ctx.emit(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(info),
                seq,
            });
            if summary_is_live(summary) {
                ctx.listener.follow(&asid.0);
            } else {
                ctx.listener.unfollow(&asid.0);
            }
        }
        "api-session/removed" => {
            // The session left the live registry; it is not deleted, so the
            // mirror keeps what it knows.
            if let Some(asid) = session_arg(0) {
                ctx.listener.unfollow(&asid.0);
                ctx.attempts.lock().await.remove(&asid.0);
            }
        }
        "api-session/status" => {
            let Some(asid) = session_arg(0) else {
                return;
            };
            let running = args.get(1).and_then(Value::as_bool).unwrap_or(false);
            if running {
                ctx.set_status(&asid, AgentSessionStatus::Busy, None).await;
            } else {
                // `turn/end` has already said how the turn ended; only a
                // session still marked busy needs telling it is idle.
                let current = ctx.mirror.get_snapshot(&asid).await.map(|s| s.info.status);
                if matches!(
                    current,
                    Some(AgentSessionStatus::Busy | AgentSessionStatus::Retry)
                ) {
                    ctx.set_status(&asid, AgentSessionStatus::Idle, None).await;
                }
            }
        }
        "api-session/error" => {
            let Some(asid) = session_arg(0) else {
                return;
            };
            let message = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("agent failed");
            tracing::warn!(asid = %asid.0, message, "deepseek reported an agent failure");
            ctx.set_status(
                &asid,
                AgentSessionStatus::Failed,
                Some(mapper::map_error_message(message)),
            )
            .await;
        }
        "api-session/activity" => {
            if let Some(asid) = session_arg(0) {
                if !ctx.mirror.is_placeholder(&asid).await {
                    ctx.patch_session(&asid, SessionPatch::default()).await;
                }
            }
        }
        other => {
            tracing::trace!(event = other, "deepseek emit not mapped");
        }
    }
}

/// One `SessionFollowFrame`.
async fn handle_follow_frame(asid: &AgentSessionId, frame: &Value, ctx: &DeepseekEventContext) {
    match frame.get("type").and_then(Value::as_str) {
        Some("snapshot") => handle_snapshot(asid, frame, ctx).await,
        Some("event") => {
            if let Some(event) = frame.get("event") {
                handle_session_event(asid, event, ctx).await;
            }
        }
        Some("assistant-stream") => {
            if let Some(inner) = frame.get("frame") {
                handle_assistant_frame(asid, inner, ctx).await;
            }
        }
        other => {
            tracing::trace!(?other, "deepseek follow frame not mapped");
        }
    }
}

async fn handle_snapshot(asid: &AgentSessionId, frame: &Value, ctx: &DeepseekEventContext) {
    let header = frame.get("header").cloned().unwrap_or(Value::Null);
    let projections = frame.get("projections").cloned().unwrap_or(Value::Null);
    if let Some(info) = mapper::map_snapshot_session(&asid.0, &header, &projections) {
        let seq = ctx.mirror.update_session(info.clone()).await;
        ctx.emit(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info),
            seq,
        });
    }
    let items = mapper::map_timeline_records(frame);
    ctx.upsert_items(asid, items).await;

    // A reconnect in the middle of an attempt: the baseline carries what has
    // streamed so far, compacted.
    ctx.finish_attempt(asid).await;
    if let Some(active) = frame.pointer("/assistantStream/activeAttempt") {
        let Some(attempt_id) = active.get("attemptId").and_then(Value::as_str) else {
            return;
        };
        let started_after = active
            .get("startedAfterSeq")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        start_attempt(asid, attempt_id, started_after, ctx).await;
        for record in active
            .get("stream")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            replay_stream_record(asid, record, ctx).await;
        }
    }
}

/// One durable `SessionWireEvent`.
async fn handle_session_event(asid: &AgentSessionId, event: &Value, ctx: &DeepseekEventContext) {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let time = event.get("time").and_then(Value::as_u64).unwrap_or(0);
    let data = event.get("data").cloned().unwrap_or(Value::Null);
    match event_type {
        "turn/start" => {
            ctx.set_status(asid, AgentSessionStatus::Busy, None).await;
        }
        "turn/end" => {
            let (status, error) = mapper::map_turn_end(&data);
            ctx.finish_attempt(asid).await;
            ctx.set_status(asid, status, error).await;
        }
        "user/message" | "compaction/summary" => {
            ctx.upsert_items(asid, mapper::map_record(event)).await;
        }
        "assistant/message" | "assistant/attempt" => {
            // The committed message replaces the rows its attempt streamed.
            ctx.finish_attempt(asid).await;
            ctx.upsert_items(asid, mapper::map_record(event)).await;
            if let Some(tokens) = data.get("usage").and_then(mapper::map_usage) {
                if let Some((seq, info)) = ctx.mirror.update_usage(asid, None, Some(tokens)).await {
                    ctx.emit(AgentDomainEvent::SessionUpdated {
                        asid: asid.clone(),
                        info: Box::new(info),
                        seq,
                    });
                }
            }
        }
        "tool/call" => {
            let Some(call_id) = data.get("callId").and_then(Value::as_str) else {
                return;
            };
            let Some(owner) = ctx.tool_owner(asid, call_id).await else {
                tracing::debug!(asid = %asid.0, call_id, "tool/call for a call with no card");
                return;
            };
            ctx.upsert_tool(
                asid,
                &owner,
                call_id,
                ToolPatch {
                    name: data.get("name").and_then(Value::as_str).map(str::to_string),
                    state: Some(ToolCallStatus::Running),
                    ran_ms: Some(time),
                    ..ToolPatch::default()
                },
            )
            .await;
        }
        "tool/result" => {
            let Some(result) = mapper::map_tool_result(&data) else {
                return;
            };
            let Some(owner) = ctx.tool_owner(asid, &result.call_id).await else {
                tracing::debug!(asid = %asid.0, call_id = %result.call_id, "tool/result for a call with no card");
                return;
            };
            ctx.upsert_tool(
                asid,
                &owner,
                &result.call_id,
                ToolPatch {
                    output: result.output.clone(),
                    content: Some(result.content.clone()),
                    state: Some(if result.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    }),
                    error: result.error.clone(),
                    completed_ms: Some(time),
                    ..ToolPatch::default()
                },
            )
            .await;
        }
        "session/title" => {
            ctx.patch_session(
                asid,
                SessionPatch {
                    title: data
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ..SessionPatch::default()
                },
            )
            .await;
        }
        "model/selection" => {
            let model = data
                .get("provider")
                .and_then(Value::as_str)
                .zip(data.get("model").and_then(Value::as_str))
                .map(|(provider, model)| crate::agents::domain::ModelRef {
                    provider_id: provider.to_string(),
                    model_id: model.to_string(),
                    variant: data
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            ctx.patch_session(
                asid,
                SessionPatch {
                    model,
                    ..SessionPatch::default()
                },
            )
            .await;
        }
        "compaction/start" | "compaction/end" => {
            let failed = data.get("error").is_some();
            let status = match (event_type, failed) {
                ("compaction/start", _) => CompactionStatus::Started,
                (_, true) => CompactionStatus::Failed,
                (_, false) => CompactionStatus::Completed,
            };
            let reason = data
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string);
            let seq = ctx
                .mirror
                .record_compaction(asid, status, reason.clone(), None)
                .await;
            ctx.emit(AgentDomainEvent::CompactionChanged {
                asid: asid.clone(),
                status,
                reason,
                delta: None,
                seq,
            });
        }
        // Audit-only pairs and bookkeeping the app has no row for.
        "approval/asked"
        | "approval/decided"
        | "approval/policy"
        | "step/start"
        | "step/end"
        | "request/header"
        | "request/context"
        | "system/message"
        | "developer/message"
        | "session/end-seed"
        | "agent/inbox/spliced" => {}
        other => {
            tracing::trace!(event = other, "deepseek session event not mapped");
        }
    }
}

async fn start_attempt(
    asid: &AgentSessionId,
    attempt_id: &str,
    started_after_seq: u64,
    ctx: &DeepseekEventContext,
) {
    ctx.finish_attempt(asid).await;
    ctx.attempts.lock().await.insert(
        asid.0.clone(),
        ActiveAttempt {
            attempt_id: attempt_id.to_string(),
            message_id: mapper::provisional_message_id(started_after_seq, attempt_id),
            row_ids: Vec::new(),
        },
    );
    ctx.set_status(asid, AgentSessionStatus::Busy, None).await;
}

/// One `SessionAssistantStreamFrame`: `start`, `chunk` or `end`.
async fn handle_assistant_frame(asid: &AgentSessionId, frame: &Value, ctx: &DeepseekEventContext) {
    match frame.get("type").and_then(Value::as_str) {
        Some("start") => {
            let Some(attempt_id) = frame.get("attemptId").and_then(Value::as_str) else {
                return;
            };
            let started_after = frame
                .get("startedAfterSeq")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            start_attempt(asid, attempt_id, started_after, ctx).await;
        }
        Some("chunk") => {
            if let Some(chunk) = frame.get("chunk") {
                apply_stream_chunk(asid, chunk, ctx).await;
            }
        }
        Some("end") => {
            // A committed message has already replaced the rows through its
            // durable event; an abandoned attempt leaves nothing behind.
            ctx.finish_attempt(asid).await;
        }
        _ => {}
    }
}

/// The compact `AssistantStreamRecord`s of a reconnect baseline.
async fn replay_stream_record(asid: &AgentSessionId, record: &Value, ctx: &DeepseekEventContext) {
    let index = record.get("index").and_then(Value::as_u64).unwrap_or(0);
    let joined = |key: &str| -> String {
        record
            .get(key)
            .and_then(Value::as_array)
            .map(|parts| parts.iter().filter_map(Value::as_str).collect::<String>())
            .unwrap_or_default()
    };
    let chunk = match record.get("type").and_then(Value::as_str) {
        Some("text-chunks") => {
            json!({ "type": "text-delta", "index": index, "text": joined("texts") })
        }
        Some("reasoning-chunks") => {
            json!({ "type": "reasoning-delta", "index": index, "text": joined("texts") })
        }
        Some("tool-call-chunks") => json!({
            "type": "tool-call-delta", "index": index,
            "id": record.get("id").cloned().unwrap_or(Value::Null),
            "name": record.get("name").cloned().unwrap_or(Value::Null),
            "argumentsDelta": joined("args")
        }),
        Some("chunk") => record.get("chunk").cloned().unwrap_or(Value::Null),
        _ => return,
    };
    apply_stream_chunk(asid, &chunk, ctx).await;
}

/// One `StreamChunk` from dsh-llm, applied to the active attempt's rows.
async fn apply_stream_chunk(asid: &AgentSessionId, chunk: &Value, ctx: &DeepseekEventContext) {
    let Some(attempt) = ctx.attempts.lock().await.get(&asid.0).cloned() else {
        tracing::trace!(asid = %asid.0, "assistant chunk outside an attempt");
        return;
    };
    let message_id = attempt.message_id.as_str();
    let index = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
    let row_id = match chunk.get("type").and_then(Value::as_str) {
        Some("text-delta") | Some("reasoning-delta") => {
            let is_reasoning = chunk.get("type").and_then(Value::as_str) == Some("reasoning-delta");
            let Some(text) = chunk.get("text").and_then(Value::as_str) else {
                return;
            };
            let item_id = if is_reasoning {
                reasoning_item_id(message_id, index)
            } else {
                text_item_id(message_id, index)
            };
            let seq = ctx
                .mirror
                .append_text_delta(asid, &item_id, message_id, index, text, is_reasoning)
                .await;
            if let (Some(seq), Some(item)) =
                (seq, ctx.mirror.get_timeline_item(asid, &item_id).await)
            {
                ctx.emit(AgentDomainEvent::TimelineUpsert {
                    asid: asid.clone(),
                    items: vec![item],
                    seq,
                });
            }
            Some(item_id)
        }
        Some("tool-call-delta") => {
            let Some(call_id) = chunk.get("id").and_then(Value::as_str) else {
                return;
            };
            ctx.upsert_tool(
                asid,
                message_id,
                call_id,
                ToolPatch {
                    name: chunk
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    input_delta: chunk
                        .get("argumentsDelta")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    state: Some(ToolCallStatus::Streaming),
                    ..ToolPatch::default()
                },
            )
            .await
        }
        Some("block-end") => {
            let Some(block) = chunk.get("block") else {
                return;
            };
            match block.get("type").and_then(Value::as_str) {
                Some("tool-call") => {
                    let Some(call_id) = block.get("id").and_then(Value::as_str) else {
                        return;
                    };
                    let input = block
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(mapper::parse_tool_arguments);
                    ctx.upsert_tool(
                        asid,
                        message_id,
                        call_id,
                        ToolPatch {
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            input,
                            state: Some(ToolCallStatus::Running),
                            ..ToolPatch::default()
                        },
                    )
                    .await
                }
                Some(kind @ ("text" | "reasoning")) => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    let is_reasoning = kind == "reasoning";
                    let item_id = if is_reasoning {
                        reasoning_item_id(message_id, index)
                    } else {
                        text_item_id(message_id, index)
                    };
                    let seq = ctx
                        .mirror
                        .set_text_content(asid, &item_id, message_id, index, text, is_reasoning)
                        .await;
                    if let (Some(seq), Some(item)) =
                        (seq, ctx.mirror.get_timeline_item(asid, &item_id).await)
                    {
                        ctx.emit(AgentDomainEvent::TimelineUpsert {
                            asid: asid.clone(),
                            items: vec![item],
                            seq,
                        });
                    }
                    Some(item_id)
                }
                _ => None,
            }
        }
        Some("usage") => {
            if let Some(tokens) = chunk.get("usage").and_then(mapper::map_usage) {
                if let Some((seq, info)) = ctx.mirror.update_usage(asid, None, Some(tokens)).await {
                    ctx.emit(AgentDomainEvent::SessionUpdated {
                        asid: asid.clone(),
                        info: Box::new(info),
                        seq,
                    });
                }
            }
            None
        }
        // `block-start` and `finish` create no row.
        _ => None,
    };
    if let Some(row_id) = row_id {
        if let Some(active) = ctx.attempts.lock().await.get_mut(&asid.0) {
            if active.attempt_id == attempt.attempt_id && !active.row_ids.contains(&row_id) {
                active.row_ids.push(row_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Frame shapes here follow the harness source: `SessionFollowFrame`
    //! and `SessionControlFrame` in dsh-api-session-controller's
    //! `types.d.ts`, the `$events` frames in dsh-api-gateway's
    //! `openRemoteEvents`/`startRemoteEvent`, and `StreamChunk` in dsh-llm.
    use super::*;
    use crate::agents::adapters::deepseek::DeepseekEndpoint;
    use crate::agents::domain::PermissionDecision;

    fn ctx() -> (
        Arc<DeepseekEventContext>,
        broadcast::Receiver<AgentDomainEvent>,
    ) {
        // Port 1 answers nothing: every test here is about what a frame does
        // to the mirror, and a seed or refetch that fails is logged.
        let endpoint = DeepseekEndpoint::new("http://127.0.0.1:1", None, None);
        let driver = Arc::new(DeepseekDriver::new(endpoint.clone()));
        let mirror = Arc::new(MemoryMirror::new());
        let (tx, rx) = broadcast::channel(256);
        let (listener, _) = DeepseekStreamListener::new(endpoint);
        let interactions = Arc::new(DeepseekInteractions::default());
        (
            Arc::new(DeepseekEventContext::new(
                driver,
                mirror,
                tx,
                Arc::new(listener),
                interactions,
            )),
            rx,
        )
    }

    fn drain(rx: &mut broadcast::Receiver<AgentDomainEvent>) -> Vec<String> {
        let mut names = Vec::new();
        while let Ok(event) = rx.try_recv() {
            names.push(event.event_name().to_string());
        }
        names
    }

    async fn follow(ctx: &DeepseekEventContext, asid: &str, frame: Value) {
        handle_deepseek_event(
            DeepseekStreamEvent::Follow {
                session_id: asid.to_string(),
                frame,
            },
            ctx,
        )
        .await;
    }

    fn event(event_type: &str, seq: u64, data: Value) -> Value {
        json!({ "type": "event", "event": { "type": event_type, "seq": seq, "time": 1000 + seq, "data": data } })
    }

    fn assistant_frame(frame: Value) -> Value {
        json!({ "type": "assistant-stream", "frame": frame })
    }

    #[tokio::test]
    async fn a_snapshot_seeds_the_session_and_its_recent_rows() {
        let (ctx, mut rx) = ctx();
        let snapshot = json!({
            "type": "snapshot",
            "header": { "version": 4, "id": "ses_1", "createdAt": 1, "cwd": "/w", "isSeeded": false },
            "cursor": 5, "hasMore": false,
            "records": [
                { "type": "event", "event": { "type": "user/message", "seq": 5, "time": 5, "surfaceOp": "append",
                    "data": { "id": "u", "role": "user", "content": [ { "type": "text", "text": "hi" } ], "source": { "kind": "user" } } } }
            ],
            "projections": { "asOfSeq": 5, "values": { "title": "Greeting" } }
        });
        follow(&ctx, "ses_1", snapshot).await;
        let asid = AgentSessionId("ses_1".into());
        let snap = ctx.mirror.get_snapshot(&asid).await.expect("session known");
        assert_eq!(snap.info.title, "Greeting");
        assert_eq!(snap.info.directory.as_deref(), Some("/w"));
        assert_eq!(snap.timeline.len(), 1);
        assert_eq!(snap.timeline[0].id, text_item_id("s000000000005", 0));
        assert_eq!(
            drain(&mut rx),
            vec!["agent.session.updated", "agent.timeline.upsert"]
        );
    }

    #[tokio::test]
    async fn a_turn_streams_then_commits_under_the_durable_id() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("ses_1".into());
        follow(&ctx, "ses_1", event("turn/start", 6, json!({ "turn": 1 }))).await;
        assert_eq!(
            ctx.mirror.get_snapshot(&asid).await.unwrap().info.status,
            AgentSessionStatus::Busy
        );
        assert_eq!(drain(&mut rx), vec!["agent.status.changed"]);

        follow(&ctx, "ses_1", assistant_frame(json!({
            "type": "start", "attemptId": "att-1", "revision": 1, "startedAfterSeq": 7, "turn": 1, "step": 1 }))).await;
        for (i, chunk) in [
            json!({ "type": "block-start", "index": 0, "blockType": "reasoning" }),
            json!({ "type": "reasoning-delta", "index": 0, "text": "hmm" }),
            json!({ "type": "block-start", "index": 1, "blockType": "text" }),
            json!({ "type": "text-delta", "index": 1, "text": "Hel" }),
            json!({ "type": "text-delta", "index": 1, "text": "lo" }),
            json!({ "type": "tool-call-delta", "index": 2, "id": "call_1", "name": "bash", "argumentsDelta": "{\"command\":" }),
            json!({ "type": "tool-call-delta", "index": 2, "id": "call_1", "argumentsDelta": "\"ls\"}" }),
            json!({ "type": "block-end", "index": 2, "block": { "type": "tool-call", "id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}" } }),
        ]
        .into_iter()
        .enumerate()
        {
            follow(&ctx, "ses_1", assistant_frame(json!({
                "type": "chunk", "attemptId": "att-1", "revision": 1, "index": i, "time": 1, "chunk": chunk }))).await;
        }
        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        let live_id = mapper::provisional_message_id(7, "att-1");
        assert_eq!(snap.timeline.len(), 3, "reasoning, text and the tool card");
        assert!(snap.timeline.iter().all(|it| it.message_id == live_id));
        let text = snap
            .timeline
            .iter()
            .find(|it| it.id == text_item_id(&live_id, 1))
            .unwrap();
        assert_eq!(
            text.part,
            AgentPart::Text {
                text: "Hello".into()
            }
        );
        let AgentPart::Tool(call) = &snap
            .timeline
            .iter()
            .find(|it| matches!(it.part, AgentPart::Tool(_)))
            .unwrap()
            .part
        else {
            unreachable!()
        };
        assert_eq!(call.name, "bash");
        assert_eq!(call.input, json!({ "command": "ls" }));
        assert_eq!(call.state, ToolCallStatus::Running);
        assert!(
            call.input_partial.is_none(),
            "the real input replaced the preview"
        );
        let names = drain(&mut rx);
        assert_eq!(
            names[0], "agent.status.changed",
            "the attempt start says busy"
        );
        assert!(
            names[1..].iter().all(|n| n == "agent.timeline.upsert"),
            "{names:?}"
        );
        assert_eq!(names.len(), 7, "one row event per chunk that touched a row");

        // The durable message commits at seq 9, then the end frame follows.
        follow(&ctx, "ses_1", event("assistant/message", 9, json!({
            "turn": 1, "step": 1,
            "message": { "id": "m", "role": "assistant", "content": [
                { "type": "reasoning", "text": "hmm" }, { "type": "text", "text": "Hello" },
                { "type": "tool-call", "id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}" } ],
                "source": { "kind": "model", "provider": "deepseek", "model": "deepseek-chat" } },
            "stream": [], "usage": { "inputTokens": 12, "outputTokens": 4 } }))).await;
        follow(
            &ctx,
            "ses_1",
            assistant_frame(json!({
            "type": "end", "attemptId": "att-1", "revision": 1, "index": 8,
            "outcome": { "kind": "committed", "eventType": "assistant/message", "seq": 9 } })),
        )
        .await;

        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.timeline.len(), 3, "the provisional rows are gone");
        assert!(snap
            .timeline
            .iter()
            .all(|it| it.message_id == "s000000000009"));
        assert_eq!(snap.info.tokens.as_ref().map(|t| t.input), Some(12));
        let names = drain(&mut rx);
        assert_eq!(
            names,
            vec![
                "agent.timeline.removed",
                "agent.timeline.upsert",
                "agent.session.updated"
            ]
        );

        // The call is dispatched, answered, and the turn ends.
        follow(&ctx, "ses_1", event("tool/call", 10, json!({ "turn": 1, "step": 1, "callId": "call_1", "name": "bash", "arguments": "{}" }))).await;
        follow(&ctx, "ses_1", event("tool/result", 11, json!({ "turn": 1, "step": 1,
            "message": { "id": "t", "role": "tool", "toolCallId": "call_1", "content": [ { "type": "text", "text": "a.rs" } ], "source": { "kind": "tool" } } }))).await;
        follow(
            &ctx,
            "ses_1",
            event(
                "turn/end",
                12,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
            ),
        )
        .await;
        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        let card = snap
            .timeline
            .iter()
            .find(|it| it.id == crate::agents::domain::tool_item_id("s000000000009", "call_1"))
            .expect("the card kept its id");
        let AgentPart::Tool(call) = &card.part else {
            unreachable!()
        };
        assert_eq!(call.state, ToolCallStatus::Completed);
        assert_eq!(call.output, Some(json!("a.rs")));
        assert_eq!(call.time.ran, Some(1010));
        assert_eq!(call.time.completed, Some(1011));
        assert_eq!(snap.info.status, AgentSessionStatus::Idle);
        assert_eq!(
            drain(&mut rx),
            vec![
                "agent.timeline.upsert",
                "agent.timeline.upsert",
                "agent.status.changed"
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_turn_and_a_host_error_are_failures_and_a_cancel_is_an_interrupt() {
        let (ctx, mut rx) = ctx();
        let asid = AgentSessionId("ses_1".into());
        follow(&ctx, "ses_1", event("turn/end", 3, json!({ "turn": 1, "reason": { "kind": "error", "error": { "code": "E", "message": "quota" } } }))).await;
        let info = ctx.mirror.get_snapshot(&asid).await.unwrap().info;
        assert_eq!(info.status, AgentSessionStatus::Failed);
        assert_eq!(info.error.unwrap().message, "quota");

        // A later "not running" does not paper over the failure.
        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Emit {
                event: "api-session/status".into(),
                args: vec![json!("ses_1"), json!(false)],
            }),
            &ctx,
        )
        .await;
        assert_eq!(
            ctx.mirror.get_snapshot(&asid).await.unwrap().info.status,
            AgentSessionStatus::Failed
        );

        follow(
            &ctx,
            "ses_1",
            event(
                "turn/end",
                4,
                json!({ "turn": 2, "reason": { "kind": "aborted", "reason": "cancel" } }),
            ),
        )
        .await;
        assert_eq!(
            ctx.mirror.get_snapshot(&asid).await.unwrap().info.status,
            AgentSessionStatus::Interrupted
        );

        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Emit {
                event: "api-session/error".into(),
                args: vec![json!("ses_1"), json!("agent crashed")],
            }),
            &ctx,
        )
        .await;
        let info = ctx.mirror.get_snapshot(&asid).await.unwrap().info;
        assert_eq!(info.status, AgentSessionStatus::Failed);
        assert_eq!(info.error.unwrap().message, "agent crashed");
        assert!(drain(&mut rx).iter().all(|n| n == "agent.status.changed"));
    }

    #[tokio::test]
    async fn an_added_live_session_is_followed_and_a_removed_one_is_not() {
        let (ctx, mut rx) = ctx();
        let summary = json!({ "sessionId": "ses_2", "agentAvailable": true, "running": false,
                              "updatedAt": 5, "blank": false, "cwd": "/p",
                              "projections": { "kind": "sequenced", "asOfSeq": 1, "values": { "title": "Two" } } });
        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Emit {
                event: "api-session/added".into(),
                args: vec![summary],
            }),
            &ctx,
        )
        .await;
        assert_eq!(ctx.listener.followed(), vec!["ses_2".to_string()]);
        let info = ctx
            .mirror
            .get_snapshot(&AgentSessionId("ses_2".into()))
            .await
            .unwrap()
            .info;
        assert_eq!(info.title, "Two");
        assert_eq!(drain(&mut rx), vec!["agent.session.updated"]);

        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Emit {
                event: "api-session/removed".into(),
                args: vec![json!("ses_2")],
            }),
            &ctx,
        )
        .await;
        assert!(ctx.listener.followed().is_empty());
        assert!(
            !ctx.mirror
                .get_snapshot(&AgentSessionId("ses_2".into()))
                .await
                .unwrap()
                .info
                .deleted,
            "leaving the live registry is not deletion"
        );
    }

    #[tokio::test]
    async fn approvals_and_questions_are_pending_until_the_host_withdraws_them() {
        let (ctx, mut rx) = ctx();
        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Ready {
                client_id: "gen-1".into(),
            }),
            &ctx,
        )
        .await;
        assert_eq!(ctx.interactions.client_id().as_deref(), Some("gen-1"));

        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Waterfall {
                event: "approval/request".into(),
                event_id: "evt-a".into(),
                agent_id: "ses_1".into(),
                request: json!({ "toolName": "bash", "callId": "call_1", "reason": "hook" }),
            }),
            &ctx,
        )
        .await;
        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Waterfall {
                event: "user-questions/request".into(),
                event_id: "evt-q".into(),
                agent_id: "ses_1".into(),
                request: json!({ "questions": [ { "id": "go", "question": "Proceed?", "options": [ { "label": "Yes" } ] } ] }),
            }),
            &ctx,
        )
        .await;
        let asid = AgentSessionId("ses_1".into());
        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.permissions.len(), 1);
        assert_eq!(snap.permissions[0].id, "evt-a");
        assert_eq!(
            snap.permissions[0].options[0].decision,
            PermissionDecision::Allow
        );
        assert_eq!(snap.forms.len(), 1);
        assert_eq!(snap.forms[0].id, "evt-q");
        assert_eq!(ctx.interactions.approvals_for("ses_1").len(), 1);
        assert_eq!(ctx.interactions.questions_for("ses_1").len(), 1);
        assert_eq!(
            drain(&mut rx),
            vec!["agent.permission.pending", "agent.form.pending"]
        );

        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Cancel {
                event_id: "evt-a".into(),
            }),
            &ctx,
        )
        .await;
        handle_deepseek_event(
            DeepseekStreamEvent::Events(EventsFrame::Cancel {
                event_id: "evt-q".into(),
            }),
            &ctx,
        )
        .await;
        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        assert!(snap.permissions.is_empty() && snap.forms.is_empty());
        assert!(ctx.interactions.approvals_for("ses_1").is_empty());
        assert_eq!(
            drain(&mut rx),
            vec!["agent.permission.resolved", "agent.form.resolved"]
        );
    }

    /// The driver answers from the same registry the stream fills; without a
    /// live `$events` generation there is nothing to answer to, and an id the
    /// host never forwarded is refused before any call is made.
    #[tokio::test]
    async fn the_driver_refuses_replies_it_cannot_deliver() {
        use crate::agents::ports::engine::{AgentEngineError, AgentEnginePort};
        let endpoint = DeepseekEndpoint::new("http://127.0.0.1:1", None, None);
        let driver = DeepseekDriver::new(endpoint.clone());
        let registry = DeepseekInteractions::for_endpoint(&endpoint.url);
        assert!(driver
            .get_pending_permissions("ses_1")
            .await
            .unwrap()
            .is_empty());

        let unknown = driver
            .reply_permission("ses_1", "evt-none", PermissionDecision::Allow, None)
            .await;
        assert!(matches!(unknown, Err(AgentEngineError::SessionNotFound(_))));

        let request =
            mapper::map_approval_request("evt-1", "ses_1", &json!({ "toolName": "bash" }));
        registry.add_approval(
            "evt-1",
            PendingApproval {
                asid: AgentSessionId("ses_1".into()),
                request,
            },
        );
        assert_eq!(
            driver.get_pending_permissions("ses_1").await.unwrap().len(),
            1
        );
        assert!(driver
            .get_pending_permissions("ses_other")
            .await
            .unwrap()
            .is_empty());

        let wrong_session = driver
            .reply_permission("ses_other", "evt-1", PermissionDecision::Allow, None)
            .await;
        assert!(matches!(
            wrong_session,
            Err(AgentEngineError::SessionNotFound(_))
        ));

        let offline = driver
            .reply_permission("ses_1", "evt-1", PermissionDecision::Allow, None)
            .await;
        assert!(matches!(offline, Err(AgentEngineError::NotAvailable(_))));
        assert!(
            registry.approval("evt-1").is_some(),
            "a reply that was not delivered leaves the request pending"
        );

        // With a generation the call is made; nothing answers on port 1, so
        // it fails as a network error and the request still stands.
        registry.begin_generation("gen-1".into());
        let refused = driver
            .reply_permission("ses_1", "evt-1", PermissionDecision::Deny, None)
            .await;
        assert!(
            matches!(refused, Err(AgentEngineError::Network(_))),
            "{refused:?}"
        );
        assert!(registry.approval("evt-1").is_some());
        registry.remove("evt-1");
    }

    #[tokio::test]
    async fn a_reconnect_baseline_replays_the_attempt_so_far() {
        let (ctx, _rx) = ctx();
        let snapshot = json!({
            "type": "snapshot",
            "header": { "version": 4, "id": "ses_1", "createdAt": 1, "isSeeded": false },
            "cursor": 20, "records": [], "hasMore": false,
            "projections": { "asOfSeq": 20, "values": {} },
            "assistantStream": { "revision": 3, "activeAttempt": {
                "attemptId": "att-9", "startedAfterSeq": 20, "turn": 2, "step": 1, "nextIndex": 4,
                "stream": [
                    { "type": "chunk", "time": 1, "chunk": { "type": "block-start", "index": 0, "blockType": "text" } },
                    { "type": "text-chunks", "time0": 1, "index": 0, "dt": [0, 1], "texts": ["Wor", "king"] },
                    { "type": "tool-call-chunks", "time0": 2, "index": 1, "dt": [0], "id": "call_7", "name": "edit", "args": ["{\"path\":"] }
                ] } }
        });
        follow(&ctx, "ses_1", snapshot).await;
        let asid = AgentSessionId("ses_1".into());
        let snap = ctx.mirror.get_snapshot(&asid).await.unwrap();
        let live_id = mapper::provisional_message_id(20, "att-9");
        let text = snap
            .timeline
            .iter()
            .find(|it| it.id == text_item_id(&live_id, 0))
            .expect("the text so far");
        assert_eq!(
            text.part,
            AgentPart::Text {
                text: "Working".into()
            }
        );
        let AgentPart::Tool(call) = &snap
            .timeline
            .iter()
            .find(|it| matches!(it.part, AgentPart::Tool(_)))
            .unwrap()
            .part
        else {
            unreachable!()
        };
        assert_eq!(call.name, "edit");
        assert_eq!(call.state, ToolCallStatus::Streaming);
        assert_eq!(call.input_partial.as_deref(), Some("{\"path\":"));
        assert_eq!(snap.info.status, AgentSessionStatus::Busy);

        // An abandoned attempt leaves no rows behind.
        follow(&ctx, "ses_1", assistant_frame(json!({
            "type": "end", "attemptId": "att-9", "revision": 3, "index": 4, "outcome": { "kind": "abandoned" } }))).await;
        assert!(ctx
            .mirror
            .get_snapshot(&asid)
            .await
            .unwrap()
            .timeline
            .is_empty());
    }
}
