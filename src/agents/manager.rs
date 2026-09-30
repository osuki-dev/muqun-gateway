use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

use serde_json::{json, Value};

use super::adapters::memory_mirror::{MemoryMirror, SessionPatch, ToolPatch};
use super::adapters::opencode::{
    mapper, OpencodeDriver, OpencodeEndpoint, OpencodeRawEvent, OpencodeSseListener,
};
use super::domain::{
    reasoning_item_id, text_item_id, AgentDomainEvent, AgentPart, AgentSessionId,
    AgentSessionStatus, CompactionStatus, RevertState, TimelineItem, TimelineRole, ToolCallStatus,
    WorktreeState,
};
use super::ports::agent::AgentPort;
use super::ports::mirror::SessionMirrorPort;
use super::use_cases::{InteractionService, PromptService, SessionService};

/// How many messages the safety-net refetch at the end of a run reads back.
/// The timeline is otherwise assembled from the stream, so this only has to
/// cover the turn that just finished.
const END_OF_RUN_REFETCH_LIMIT: usize = 20;

/// Shell ids are remembered only so `shell.exited` -- which carries no session
/// -- can be attributed. Bounded, because a long-lived gateway sees many.
const MAX_TRACKED_SHELLS: usize = 256;

pub struct AgentManager {
    agent: Arc<dyn AgentPort>,
    mirror: Arc<MemoryMirror>,
    session_service: Arc<SessionService>,
    prompt_service: Arc<PromptService>,
    interaction_service: Arc<InteractionService>,
    events_tx: broadcast::Sender<AgentDomainEvent>,
    endpoint_url: String,
    endpoint_version: Option<String>,
    stream_connected: Arc<dyn Fn() -> bool + Send + Sync>,
    shutdown_handle: Arc<dyn Fn() + Send + Sync>,
}

/// What a `shell.created` told us, kept until the matching `shell.exited`.
#[derive(Clone)]
struct ShellOwner {
    asid: AgentSessionId,
    command: String,
}

#[derive(Default)]
struct ShellRegistry {
    owners: Mutex<HashMap<String, ShellOwner>>,
}

impl ShellRegistry {
    async fn remember(&self, shell_id: &str, owner: ShellOwner) {
        let mut owners = self.owners.lock().await;
        if owners.len() >= MAX_TRACKED_SHELLS {
            // Oldest-first is not worth a second index here; dropping an
            // arbitrary entry only costs one unattributed `shell.exited`.
            if let Some(key) = owners.keys().next().cloned() {
                owners.remove(&key);
            }
        }
        owners.insert(shell_id.to_string(), owner);
    }

    async fn take(&self, shell_id: &str) -> Option<ShellOwner> {
        self.owners.lock().await.remove(shell_id)
    }
}

/// Everything `handle_raw_event` needs, so the dispatch table stays readable.
pub(crate) struct EventContext {
    pub driver: Arc<OpencodeDriver>,
    pub mirror: Arc<MemoryMirror>,
    pub tx: broadcast::Sender<AgentDomainEvent>,
    shells: ShellRegistry,
}

impl EventContext {
    pub(crate) fn new(
        driver: Arc<OpencodeDriver>,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Self {
        Self {
            driver,
            mirror,
            tx,
            shells: ShellRegistry::default(),
        }
    }

    fn emit(&self, mut event: AgentDomainEvent) {
        self.mirror.stamp_event(&mut event);
        // A send with no subscribers is not a failure: nobody is watching.
        let _ = self.tx.send(event);
    }

    /// Make sure the mirror holds real session info before an event is
    /// attributed to it. Without this the first thing a client learns about a
    /// new session is the local placeholder.
    async fn ensure_session(&self, asid: &AgentSessionId) {
        if !self.mirror.is_placeholder(asid).await {
            return;
        }
        match self.driver.get_session(&asid.0).await {
            Ok(info) => {
                let seq = self.mirror.update_session(info.clone()).await;
                self.emit(AgentDomainEvent::SessionUpdated {
                    asid: asid.clone(),
                    info: Box::new(info),
                    seq,
                });
            }
            Err(err) => {
                tracing::debug!(asid = %asid.0, %err, "could not hydrate unknown session");
            }
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

    async fn set_status(
        &self,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
        error: Option<crate::agents::domain::AgentErrorInfo>,
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

    async fn upsert_tool(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
        call_id: &str,
        patch: ToolPatch,
    ) {
        match self
            .mirror
            .upsert_tool_call(asid, message_id, call_id, patch)
            .await
        {
            Some((seq, item)) => self.emit(AgentDomainEvent::TimelineUpsert {
                asid: asid.clone(),
                items: vec![item],
                seq,
            }),
            None => {
                tracing::debug!(asid = %asid.0, call_id, "tool event dropped: not a tool row");
            }
        }
    }

    async fn emit_timeline_item(&self, asid: &AgentSessionId, item: TimelineItem) {
        let seq = self
            .mirror
            .upsert_timeline_items(asid, vec![item.clone()])
            .await;
        self.emit(AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: vec![item],
            seq,
        });
    }

    /// The one refetch left in the event path: a safety net at the end of a
    /// run, reading back only the turn that just finished rather than a
    /// hundred messages at every step boundary.
    async fn refetch_tail(&self, asid: &AgentSessionId) {
        match self
            .driver
            .get_timeline(&asid.0, END_OF_RUN_REFETCH_LIMIT)
            .await
        {
            Ok(items) if !items.is_empty() => {
                let seq = self.mirror.upsert_timeline_items(asid, items.clone()).await;
                self.emit(AgentDomainEvent::TimelineUpsert {
                    asid: asid.clone(),
                    items,
                    seq,
                });
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(asid = %asid.0, %err, "end-of-run timeline refetch failed");
            }
        }
    }
}

impl AgentManager {
    /// Attempt to discover and initialize available agents (such as OpenCode V2).
    pub async fn discover() -> Option<Self> {
        let endpoint = OpencodeEndpoint::discover().await?;
        let (events_tx, _) = broadcast::channel(1024);
        Some(Self::connect(endpoint, events_tx))
    }

    /// Build a manager for an endpoint that has already been discovered, on a
    /// caller-owned event channel so subscribers survive a reconnect.
    pub fn connect(
        endpoint: OpencodeEndpoint,
        events_tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Self {
        let endpoint_url = endpoint.url.clone();
        let endpoint_version = endpoint.version.clone();
        let driver = Arc::new(OpencodeDriver::new(endpoint.clone()));
        let mirror = Arc::new(MemoryMirror::for_agent(driver.kind()));

        let (listener, mut sse_rx) = OpencodeSseListener::new(endpoint);
        let listener = Arc::new(listener);
        listener.start();

        let ctx = Arc::new(EventContext::new(
            driver.clone(),
            mirror.clone(),
            events_tx.clone(),
        ));

        let pump_listener = listener.clone();
        tokio::spawn(async move {
            loop {
                match sse_rx.recv().await {
                    Ok(raw_event) => {
                        Self::handle_raw_event(raw_event, &ctx).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Dropping out of the loop here used to stop the
                        // gateway processing OpenCode events for the rest of
                        // the process lifetime. Tell the clients to resync and
                        // keep reading.
                        tracing::warn!(skipped, "opencode event backlog overflowed, resyncing");
                        ctx.emit(AgentDomainEvent::Resync {
                            asid: AgentSessionId(String::new()),
                            reason: "event_backlog_overflow".to_string(),
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Every manager owns its listener and its channel, so
                        // a re-discovery stops the old listener and this is
                        // the old pump noticing. That is the hand-over working
                        // -- it used to be logged as a warning, half a second
                        // after the replacement stream had already connected,
                        // which read like the new agent had failed.
                        //
                        // A channel that closes while the listener was still
                        // meant to be reading is a different thing and keeps
                        // its warning.
                        if pump_listener.is_running() {
                            tracing::warn!(
                                "opencode event channel closed while still listening, \
                                 stopping event pump"
                            );
                        } else {
                            tracing::debug!(
                                "previous opencode event pump wound down after hand-over"
                            );
                        }
                        break;
                    }
                }
            }
        });

        tracing::info!(
            url = %endpoint_url,
            version = endpoint_version.as_deref().unwrap_or("unknown"),
            "agent manager initialized with the OpenCode agent"
        );
        let stream_listener = listener.clone();
        let shutdown_listener = listener.clone();
        Self::assemble(
            driver,
            mirror,
            events_tx,
            endpoint_url,
            endpoint_version,
            Arc::new(move || stream_listener.is_connected()),
            Arc::new(move || shutdown_listener.stop()),
        )
    }

    /// The part every agent's manager has in common: the use-case services
    /// over its driver and mirror, and the handles to its event stream.
    fn assemble(
        agent: Arc<dyn AgentPort>,
        mirror: Arc<MemoryMirror>,
        events_tx: broadcast::Sender<AgentDomainEvent>,
        endpoint_url: String,
        endpoint_version: Option<String>,
        stream_connected: Arc<dyn Fn() -> bool + Send + Sync>,
        shutdown_handle: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            session_service: Arc::new(SessionService::with_memory_mirror(
                agent.clone(),
                mirror.clone(),
            )),
            prompt_service: Arc::new(PromptService::new(agent.clone(), mirror.clone())),
            interaction_service: Arc::new(InteractionService::new(agent.clone(), mirror.clone())),
            agent,
            mirror,
            events_tx,
            endpoint_url,
            endpoint_version,
            stream_connected,
            shutdown_handle,
        }
    }

    /// Build a manager for a DeepSeek endpoint.
    pub fn connect_deepseek(
        endpoint: crate::agents::adapters::deepseek::DeepseekEndpoint,
        events_tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Self {
        let endpoint_url = endpoint.url.clone();
        let endpoint_version = endpoint.version.clone();
        let driver = Arc::new(crate::agents::adapters::deepseek::DeepseekDriver::new(
            endpoint.clone(),
        ));
        let mirror = Arc::new(MemoryMirror::for_agent(driver.kind()));

        // The pending approvals and the `$events` client id are shared with
        // the driver through the per-endpoint registry, because the driver
        // is what answers them and it is built before the listener.
        let interactions =
            crate::agents::adapters::deepseek::stream::DeepseekInteractions::for_endpoint(
                &endpoint_url,
            );
        let (listener, mut stream_rx) =
            crate::agents::adapters::deepseek::DeepseekStreamListener::new(endpoint);
        let listener = Arc::new(listener);
        listener.start();

        let ctx = Arc::new(deepseek::DeepseekEventContext::new(
            driver.clone(),
            mirror.clone(),
            events_tx.clone(),
            listener.clone(),
            interactions,
        ));

        let pump_listener = listener.clone();
        tokio::spawn(async move {
            loop {
                match stream_rx.recv().await {
                    Ok(event) => {
                        deepseek::handle_deepseek_event(event, &ctx).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "deepseek event backlog overflowed, resyncing");
                        ctx.emit(AgentDomainEvent::Resync {
                            asid: AgentSessionId(String::new()),
                            reason: "event_backlog_overflow".to_string(),
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        if pump_listener.is_running() {
                            tracing::warn!(
                                "deepseek event channel closed while still listening, \
                                 stopping event pump"
                            );
                        } else {
                            tracing::debug!(
                                "previous deepseek event pump wound down after hand-over"
                            );
                        }
                        break;
                    }
                }
            }
        });

        let stream_listener = listener.clone();
        let shutdown_listener = listener.clone();

        tracing::info!(
            url = %endpoint_url,
            version = endpoint_version.as_deref().unwrap_or("unknown"),
            "agent manager initialized with the DeepSeek agent"
        );

        Self::assemble(
            driver,
            mirror,
            events_tx,
            endpoint_url,
            endpoint_version,
            Arc::new(move || stream_listener.is_connected()),
            Arc::new(move || shutdown_listener.stop()),
        )
    }

    /// Build a manager for a T3 Code server. `endpoint` carries the
    /// credential; `version` is the server version its descriptor reported.
    pub fn connect_t3(
        endpoint: crate::agents::adapters::t3::T3Endpoint,
        runtime_mode: Option<&str>,
        version: Option<String>,
        events_tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Self {
        use crate::agents::adapters::t3::client::RUNTIME_MODES;
        use crate::agents::adapters::t3::{T3Client, T3Driver, T3StreamListener};
        let endpoint_url = endpoint.url.clone();
        let client = Arc::new(T3Client::new(endpoint));
        let mut listener = T3StreamListener::new(client.clone());
        let watcher = listener.watcher();
        let mut stream_rx = listener.start();

        let mut driver = T3Driver::from_client(client.clone()).with_watcher(watcher.clone());
        if let Some(mode) = runtime_mode.map(str::trim) {
            if RUNTIME_MODES.contains(&mode) {
                driver = driver
                    .with_runtime_mode(mode)
                    .unwrap_or_else(|_| unreachable!());
            } else {
                tracing::warn!(
                    runtime_mode = mode,
                    "t3.runtime_mode is not a T3 runtime mode; using full-access"
                );
            }
        }
        let driver = Arc::new(driver);
        let mirror = Arc::new(MemoryMirror::for_agent(driver.kind()));
        let ctx = Arc::new(t3::T3EventContext::new(
            driver.clone(),
            mirror.clone(),
            events_tx.clone(),
            watcher,
        ));
        tokio::spawn(async move {
            // The listener owns the sending side; it ends when the manager
            // is shut down, and so does this pump.
            while let Some(event) = stream_rx.recv().await {
                t3::handle_t3_event(event, &ctx).await;
            }
            tracing::debug!("t3 event pump wound down");
        });

        tracing::info!(
            url = %endpoint_url,
            version = version.as_deref().unwrap_or("unknown"),
            "agent manager initialized with the T3 Code agent"
        );
        let socket = client.clone();
        let cancel = listener.cancel_token();
        Self::assemble(
            driver,
            mirror,
            events_tx,
            endpoint_url,
            version,
            Arc::new(move || socket.rpc().is_connected()),
            Arc::new(move || {
                cancel.cancel();
                client.shutdown();
            }),
        )
    }

    /// A manager over a caller-supplied agent with no event stream, for
    /// route and service tests.
    #[cfg(test)]
    pub(crate) fn for_test(agent: Arc<dyn AgentPort>) -> Self {
        let mirror = Arc::new(MemoryMirror::for_agent(agent.kind()));
        let (events_tx, _) = broadcast::channel(16);
        Self::assemble(
            agent,
            mirror,
            events_tx,
            "http://127.0.0.1:1".to_string(),
            Some("test".to_string()),
            Arc::new(|| false),
            Arc::new(|| {}),
        )
    }

    pub fn is_available(&self) -> bool {
        true
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events_tx.subscribe()
    }

    pub fn sessions(&self) -> &SessionService {
        &self.session_service
    }

    pub fn prompts(&self) -> &PromptService {
        &self.prompt_service
    }

    pub fn interactions(&self) -> &InteractionService {
        &self.interaction_service
    }

    pub async fn view_session(
        &self,
        asid: &str,
        viewed: u64,
    ) -> Result<(), super::ports::agent::AgentError> {
        use super::ports::agent::AgentError;
        match self.agent.view_session(asid, viewed).await {
            Ok(()) | Err(AgentError::Unsupported(_)) => {}
            Err(err) => return Err(err),
        }
        let id = AgentSessionId(asid.to_string());
        if self.mirror.is_placeholder(&id).await {
            let info = self.agent.get_session(asid).await?;
            self.mirror.update_session(info).await;
        }
        if let Some((seq, info)) = self.mirror.mark_viewed(&id, viewed).await {
            let mut event = AgentDomainEvent::SessionUpdated {
                asid: id,
                info: Box::new(info),
                seq,
            };
            self.mirror.stamp_event(&mut event);
            let _ = self.events_tx.send(event);
        }
        Ok(())
    }

    pub fn agent(&self) -> &Arc<dyn AgentPort> {
        &self.agent
    }

    pub fn mirror(&self) -> &Arc<MemoryMirror> {
        &self.mirror
    }

    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    pub fn version(&self) -> Option<String> {
        self.endpoint_version.clone()
    }

    /// True while the event reader has an open stream to the agent.
    pub fn stream_connected(&self) -> bool {
        (self.stream_connected)()
    }

    pub fn shutdown(&self) {
        (self.shutdown_handle)();
    }

    pub(crate) async fn handle_raw_event(raw: OpencodeRawEvent, ctx: &EventContext) {
        let event_type = raw.event_type.as_str();
        let data = &raw.data;
        // The envelope timestamp is the only time a tool event carries.
        let created = raw.created;

        // Every event below is attributed to a session; the ones that are not
        // are logged at debug rather than silently dropped.
        let session_id = data
            .get("sessionID")
            .and_then(Value::as_str)
            .or_else(|| data.pointer("/form/sessionID").and_then(Value::as_str))
            .or_else(|| {
                data.pointer("/info/metadata/sessionID")
                    .and_then(Value::as_str)
            });

        match event_type {
            // ---------------------------------------------------------------
            // Session lifecycle and identity
            // ---------------------------------------------------------------
            "session.created" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.patch_session(
                    &asid,
                    SessionPatch {
                        title: data
                            .get("title")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        agent: data
                            .get("agent")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        model: data.get("model").and_then(mapper::map_model_ref),
                        parent_id: data
                            .get("parentID")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        directory: data
                            .pointer("/location/directory")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        project_id: data
                            .get("projectID")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        ..SessionPatch::default()
                    },
                )
                .await;
            }
            // A move carries the new location inline -- `{sessionID, location,
            // projectID, subpath}` -- and no `info`, so without this it fell
            // through to the refetch below. The directory is the one field of
            // this event the app draws, so it is patched in directly and the
            // round trip is saved.
            "session.moved" if data.get("info").is_none() && data.get("session").is_none() => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.patch_session(
                    &asid,
                    SessionPatch {
                        directory: data
                            .pointer("/location/directory")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        project_id: data
                            .get("projectID")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        ..SessionPatch::default()
                    },
                )
                .await;
            }
            // `session.updated` carries a whole `Session.Info`.
            "session.updated" | "session.moved" | "session.forked" => {
                let Some(info_raw) = data.get("info").or_else(|| data.get("session")) else {
                    // No embedded info: fall back to a read.
                    if let Some(id) = session_id {
                        let asid = AgentSessionId(id.to_string());
                        if let Ok(info) = ctx.driver.get_session(id).await {
                            let seq = ctx.mirror.update_session(info.clone()).await;
                            ctx.emit(AgentDomainEvent::SessionUpdated {
                                asid,
                                info: Box::new(info),
                                seq,
                            });
                        }
                    }
                    return;
                };
                if let Some(info) = mapper::map_session(info_raw) {
                    let asid = info.asid.clone();
                    let seq = ctx.mirror.update_session(info.clone()).await;
                    ctx.emit(AgentDomainEvent::SessionUpdated {
                        asid,
                        info: Box::new(info),
                        seq,
                    });
                }
            }
            "session.renamed" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.patch_session(
                    &asid,
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
            "session.deleted" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.patch_session(
                    &asid,
                    SessionPatch {
                        deleted: true,
                        ..SessionPatch::default()
                    },
                )
                .await;
                ctx.mirror.remove_session(&asid).await;
            }
            "session.model.selected" | "session.model.changed" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.patch_session(
                    &asid,
                    SessionPatch {
                        model: data.get("model").and_then(mapper::map_model_ref),
                        ..SessionPatch::default()
                    },
                )
                .await;
            }
            "session.agent.selected" | "session.agent.changed" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.patch_session(
                    &asid,
                    SessionPatch {
                        agent: data
                            .get("agent")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        ..SessionPatch::default()
                    },
                )
                .await;
            }

            // ---------------------------------------------------------------
            // Run status. Only `session.execution.*` ends a turn: a
            // `session.step.ended` with `finish: "tool-calls"` is mid-run.
            // ---------------------------------------------------------------
            "session.execution.started" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.set_status(&asid, AgentSessionStatus::Busy, None).await;
            }
            "session.execution.succeeded" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.set_status(&asid, AgentSessionStatus::Idle, None).await;
                ctx.refetch_tail(&asid).await;
            }
            "session.execution.failed" | "session.error" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                let error = data.get("error").and_then(mapper::map_error);
                tracing::warn!(
                    asid = %asid.0,
                    error = error.as_ref().map(|e| e.message.as_str()).unwrap_or("unknown"),
                    "opencode reported a session failure"
                );
                ctx.set_status(&asid, AgentSessionStatus::Failed, error)
                    .await;
                ctx.refetch_tail(&asid).await;
            }
            "session.execution.interrupted" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.set_status(&asid, AgentSessionStatus::Interrupted, None)
                    .await;
                ctx.refetch_tail(&asid).await;
            }
            "session.idle" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.set_status(&asid, AgentSessionStatus::Idle, None).await;
            }
            "session.retry.scheduled" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                let error = data.get("error").and_then(mapper::map_error);
                ctx.set_status(&asid, AgentSessionStatus::Retry, error)
                    .await;
            }
            // Not emitted by 2.0.1, but its shape is documented and harmless to
            // accept from a newer server.
            "session.status" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                let status = match data.pointer("/status/type").and_then(Value::as_str) {
                    Some("busy") => AgentSessionStatus::Busy,
                    Some("retry") => AgentSessionStatus::Retry,
                    Some("idle") => AgentSessionStatus::Idle,
                    Some("failed") => AgentSessionStatus::Failed,
                    Some("interrupted") => AgentSessionStatus::Interrupted,
                    _ => AgentSessionStatus::Unknown,
                };
                let error = data.pointer("/status/error").and_then(mapper::map_error);
                ctx.set_status(&asid, status, error).await;
            }
            "session.step.ended" => {
                // Not a status change. It does carry the running cost/token
                // totals for the step, which are worth keeping.
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                Self::apply_usage(ctx, &asid, data).await;
            }
            "session.step.started" | "session.step.streamed" | "session.step.failed" => {}
            "session.usage.updated" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                Self::apply_usage(ctx, &asid, data).await;
            }
            "session.viewed" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                ctx.patch_session(&asid, SessionPatch::default()).await;
            }

            // ---------------------------------------------------------------
            // Permissions and forms
            // ---------------------------------------------------------------
            "permission.asked" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                if let Some(req) = mapper::map_permission_request(data, &asid) {
                    let seq = ctx.mirror.add_permission(req.clone()).await;
                    ctx.emit(AgentDomainEvent::PermissionPending {
                        asid,
                        request: req,
                        seq,
                    });
                } else {
                    tracing::debug!("permission.asked dropped: no id in payload");
                }
            }
            "permission.replied" | "permission.rejected" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                if let Some(req_id) = data
                    .get("requestID")
                    .or_else(|| data.get("id"))
                    .and_then(Value::as_str)
                {
                    let seq = ctx.mirror.resolve_permission(&asid, req_id).await;
                    ctx.emit(AgentDomainEvent::PermissionResolved {
                        asid,
                        request_id: req_id.to_string(),
                        seq,
                    });
                }
            }
            "form.created" => {
                // The payload is `{form: Form.Info}`; reading `sessionID` off
                // the outer object always missed and this handler never ran.
                let form = data.get("form").unwrap_or(data);
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                if let Some(req) = mapper::map_form_request(form, &asid) {
                    let seq = ctx.mirror.add_form(req.clone()).await;
                    ctx.emit(AgentDomainEvent::FormPending {
                        asid,
                        request: req,
                        seq,
                    });
                } else {
                    tracing::debug!("form.created dropped: payload had no id or fields");
                }
            }
            "form.replied" | "form.cancelled" => {
                let form = data.get("form").unwrap_or(data);
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                if let Some(form_id) = form
                    .get("formID")
                    .or_else(|| form.get("id"))
                    .and_then(Value::as_str)
                {
                    let seq = ctx.mirror.resolve_form(&asid, form_id).await;
                    ctx.emit(AgentDomainEvent::FormResolved {
                        asid,
                        form_id: form_id.to_string(),
                        seq,
                    });
                }
            }

            // ---------------------------------------------------------------
            // Streaming assistant output
            // ---------------------------------------------------------------
            "session.text.delta"
            | "session.text.ended"
            | "session.reasoning.delta"
            | "session.reasoning.ended" => {
                Self::handle_stream_text(ctx, event_type, data, session_id).await;
            }

            // ---------------------------------------------------------------
            // Tool calls, joined on the call id
            // ---------------------------------------------------------------
            "session.tool.input.started"
            | "session.tool.input.delta"
            | "session.tool.input.ended"
            | "session.tool.called"
            | "session.tool.progress"
            | "session.tool.success"
            | "session.tool.failed" => {
                Self::handle_tool_event(ctx, event_type, data, session_id, created).await;
            }

            // ---------------------------------------------------------------
            // Compaction
            // ---------------------------------------------------------------
            "session.compaction.started"
            | "session.compaction.delta"
            | "session.compaction.ended"
            | "session.compaction.failed"
            | "session.compacted" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                let status = match event_type {
                    "session.compaction.started" => CompactionStatus::Started,
                    "session.compaction.delta" => CompactionStatus::Running,
                    "session.compaction.failed" => CompactionStatus::Failed,
                    _ => CompactionStatus::Completed,
                };
                let reason = data
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let delta = data.get("text").and_then(Value::as_str).map(str::to_string);
                let seq = ctx
                    .mirror
                    .record_compaction(&asid, status, reason.clone(), delta.clone())
                    .await;
                ctx.emit(AgentDomainEvent::CompactionChanged {
                    asid: asid.clone(),
                    status,
                    reason,
                    delta,
                    seq,
                });
                // The finished boundary is a real message; read it back so the
                // timeline carries the summary rather than only the event.
                if matches!(
                    status,
                    CompactionStatus::Completed | CompactionStatus::Failed
                ) {
                    ctx.refetch_tail(&asid).await;
                }
            }

            // ---------------------------------------------------------------
            // Revert: staging a rollback, applying it, and withdrawing it
            // ---------------------------------------------------------------
            "session.revert.staged" | "session.revert.committed" | "session.revert.cleared" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                // Only `staged` carries a boundary; on the other two nothing
                // is staged any more, which is what `None` says here.
                let (revert_state, revert) = match event_type {
                    "session.revert.staged" => (
                        RevertState::Staged,
                        data.get("revert").and_then(mapper::map_revert),
                    ),
                    "session.revert.committed" => (RevertState::Committed, None),
                    _ => (RevertState::Cleared, None),
                };
                let (seq, _info) = ctx
                    .mirror
                    .record_revert(&asid, revert_state, revert.clone())
                    .await;
                ctx.emit(AgentDomainEvent::RevertChanged {
                    asid: asid.clone(),
                    state: revert_state,
                    revert,
                    seq,
                });
                // A committed rollback deletes the boundary message and
                // everything after it. 2.0.1 announces no message removal, so
                // the mirror would otherwise keep serving rows that are gone;
                // `to` on this event is the boundary.
                if let Some(boundary) = data.get("to").and_then(Value::as_str) {
                    if let Some((seq, ids)) = ctx.mirror.remove_timeline_from(&asid, boundary).await
                    {
                        ctx.emit(AgentDomainEvent::TimelineRemoved {
                            asid: asid.clone(),
                            ids,
                            seq,
                        });
                    }
                }
            }

            // ---------------------------------------------------------------
            // Skill activation
            // ---------------------------------------------------------------
            // Activating a skill appends a `skill` message, and this event is
            // the only announcement of it: 2.0.1 has no `session.message.*`
            // family, so a live capture of an activation shows this frame and
            // nothing else. Without this arm the row reached the app only on
            // the next snapshot refetch.
            //
            // The payload is `{sessionID, id, name, text}` and carries no
            // message id. The envelope's own id is that message id under an
            // `evt_` prefix rather than `msg_` -- checked against 2.0.1 over
            // five activations in three sessions -- which is what keeps the
            // row emitted here addressed identically to the one a read-back
            // produces, so the two upsert onto each other instead of becoming
            // two rows. An envelope that does not spell its id that way is
            // read back rather than guessed at.
            "session.skill.activated" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                ctx.ensure_session(&asid).await;
                let message_id = raw
                    .id
                    .as_deref()
                    .and_then(|id| id.strip_prefix("evt_"))
                    .map(|body| format!("msg_{body}"));
                let Some(message_id) = message_id else {
                    ctx.refetch_tail(&asid).await;
                    return;
                };
                // Built as the message OpenCode stored and mapped by the one
                // mapper, so the streamed row and the refetched row are the
                // same row.
                let message = json!({
                    "id": message_id,
                    "type": "skill",
                    "skill": data.get("id").cloned().unwrap_or(Value::Null),
                    "name": data.get("name").cloned().unwrap_or(Value::Null),
                    "text": data.get("text").cloned().unwrap_or(Value::Null),
                    "time": { "created": created },
                });
                for item in mapper::map_message(&message, &asid) {
                    ctx.emit_timeline_item(&asid, item).await;
                }
            }

            // ---------------------------------------------------------------
            // Inbox (queued and steered prompts)
            // ---------------------------------------------------------------
            "session.inbox.enqueued"
            | "session.inbox.delivered"
            | "session.inbox.cancelled"
            | "session.inbox.delivery.changed" => {
                let Some(asid) = session_id.map(str::to_string).map(AgentSessionId) else {
                    return;
                };
                let inbox_id = data.get("inboxID").and_then(Value::as_str).unwrap_or("");
                let item = if event_type == "session.inbox.enqueued"
                    || event_type == "session.inbox.delivery.changed"
                {
                    data.get("item").cloned()
                } else {
                    None
                };
                let (seq, items) = ctx.mirror.upsert_inbox_item(&asid, inbox_id, item).await;
                ctx.emit(AgentDomainEvent::InboxChanged { asid, items, seq });
            }

            // ---------------------------------------------------------------
            // The catalog's own surfaces changed
            // ---------------------------------------------------------------
            // A user who edits an agent file, adds a command or installs a
            // skill expects the picker to show it -- not to show it in thirty
            // seconds when the cache happens to expire. These events are what
            // makes caching the catalog honest rather than merely fast.
            "agent.updated" | "command.updated" | "skill.updated" | "catalog.updated"
            | "config.updated" | "provider.updated" => {
                ctx.driver.invalidate_catalog();
            }

            // ---------------------------------------------------------------
            // Worktrees
            // ---------------------------------------------------------------
            // None of these belongs to a session, so they carry an empty asid
            // and reach every stream, the way a global resync does.
            //
            // What 2.0.1 actually emits for a worktree created, refreshed or
            // removed over HTTP is `worktree.updated` and `worktree.resolved`
            // -- confirmed by driving a full create/list/refresh/remove cycle
            // against the live service while tailing `/api/event`.
            // `worktree.ready`, `worktree.failed` and the `workspace.*` pair
            // are in the binary's event registry with schemas `{name, branch?}`
            // and `{message}`, and none of them appeared in any of those
            // flows; they are accepted here so a build or a remote workspace
            // that does emit them is not silently dropped.
            "worktree.updated" | "worktree.resolved" | "worktree.ready" | "worktree.failed"
            | "workspace.ready" | "workspace.failed" => {
                let state = match event_type {
                    "worktree.updated" => WorktreeState::Updated,
                    "worktree.resolved" => WorktreeState::Resolved,
                    "worktree.ready" | "workspace.ready" => WorktreeState::Ready,
                    _ => WorktreeState::Failed,
                };
                // `worktree.resolved` names the directory it resolved to; the
                // rest only say which project, and the envelope's own
                // `location.directory` is the project directory.
                let directory = data
                    .get("directory")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        raw.location
                            .as_ref()
                            .and_then(|l| l.get("directory"))
                            .and_then(Value::as_str)
                    })
                    .map(str::to_string);
                ctx.emit(AgentDomainEvent::WorktreeChanged {
                    asid: AgentSessionId(String::new()),
                    state,
                    directory,
                    project_id: data
                        .get("projectID")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: data.get("name").and_then(Value::as_str).map(str::to_string),
                    branch: data
                        .get("branch")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    error: data
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }

            // ---------------------------------------------------------------
            // Background shells
            // ---------------------------------------------------------------
            "shell.created" => {
                let info = data.get("info").unwrap_or(data);
                let Some(asid) = info
                    .pointer("/metadata/sessionID")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .map(AgentSessionId)
                else {
                    tracing::debug!("shell.created dropped: no owning session");
                    return;
                };
                let Some(shell_id) = info.get("id").and_then(Value::as_str) else {
                    return;
                };
                let command = info
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                ctx.shells
                    .remember(
                        shell_id,
                        ShellOwner {
                            asid: asid.clone(),
                            command: command.clone(),
                        },
                    )
                    .await;
                ctx.emit_timeline_item(&asid, Self::shell_item(shell_id, &command, info))
                    .await;
            }
            "shell.exited" | "shell.deleted" => {
                let Some(shell_id) = data
                    .get("id")
                    .or_else(|| data.pointer("/info/id"))
                    .and_then(Value::as_str)
                else {
                    return;
                };
                let Some(owner) = ctx.shells.take(shell_id).await else {
                    tracing::debug!(shell_id, "shell exit for an unknown shell");
                    return;
                };
                ctx.emit_timeline_item(
                    &owner.asid,
                    Self::shell_item(shell_id, &owner.command, data),
                )
                .await;
            }

            other => {
                tracing::trace!(event = other, "opencode event not mapped");
            }
        }
    }

    async fn apply_usage(ctx: &EventContext, asid: &AgentSessionId, data: &Value) {
        let cost = data.get("cost").and_then(Value::as_f64);
        let tokens = data
            .get("tokens")
            .map(|t| crate::agents::domain::TokensUsage {
                input: t.get("input").and_then(Value::as_u64).unwrap_or(0),
                output: t.get("output").and_then(Value::as_u64).unwrap_or(0),
                reasoning: t.get("reasoning").and_then(Value::as_u64),
                cache_read: t.pointer("/cache/read").and_then(Value::as_u64),
                cache_write: t.pointer("/cache/write").and_then(Value::as_u64),
            });
        if cost.is_none() && tokens.is_none() {
            return;
        }
        if let Some((seq, info)) = ctx.mirror.update_usage(asid, cost, tokens).await {
            ctx.emit(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(info),
                seq,
            });
        }
    }

    fn shell_item(shell_id: &str, command: &str, raw: &Value) -> TimelineItem {
        let info = raw.get("info").unwrap_or(raw);
        let status = info
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_string();
        let updated_ms = info
            .pointer("/time/completed")
            .or_else(|| info.pointer("/time/started"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        TimelineItem {
            // Shells are their own row family; `shell_` sorts after `msg_`, so
            // they trail the conversation rather than interleaving with it.
            id: format!("shell_{shell_id}"),
            message_id: format!("shell_{shell_id}"),
            role: TimelineRole::System,
            part: AgentPart::Shell {
                shell_id: shell_id.to_string(),
                command: command.to_string(),
                status,
                exit: info.get("exit").and_then(Value::as_f64),
                output: None,
                truncated: false,
            },
            seq: 0,
            updated_ms,
            ordinal: 0,
            attachments: None,
        }
    }

    async fn handle_stream_text(
        ctx: &EventContext,
        event_type: &str,
        data: &Value,
        session_id: Option<&str>,
    ) {
        let (Some(session_id), Some(msg_id)) = (
            session_id,
            data.get("assistantMessageID")
                .or_else(|| data.get("messageID"))
                .and_then(Value::as_str),
        ) else {
            tracing::debug!(event = event_type, "stream text event without ids");
            return;
        };
        let asid = AgentSessionId(session_id.to_string());
        let is_reasoning = event_type.starts_with("session.reasoning");
        let ordinal = data.get("ordinal").and_then(Value::as_u64).unwrap_or(0);
        // Reasoning and text both start at ordinal 0 on the same message, so
        // the row id has to say which of the two it is.
        let item_id = if is_reasoning {
            reasoning_item_id(msg_id, ordinal)
        } else {
            text_item_id(msg_id, ordinal)
        };

        let seq = if event_type.ends_with(".delta") {
            let Some(delta) = data.get("delta").and_then(Value::as_str) else {
                return;
            };
            ctx.mirror
                .append_text_delta(&asid, &item_id, msg_id, ordinal, delta, is_reasoning)
                .await
        } else {
            let Some(text) = data.get("text").and_then(Value::as_str) else {
                return;
            };
            ctx.mirror
                .set_text_content(&asid, &item_id, msg_id, ordinal, text, is_reasoning)
                .await
        };

        let Some(seq) = seq else {
            return;
        };
        if let Some(item) = ctx.mirror.get_timeline_item(&asid, &item_id).await {
            ctx.emit(AgentDomainEvent::TimelineUpsert {
                asid,
                items: vec![item],
                seq,
            });
        }
    }

    async fn handle_tool_event(
        ctx: &EventContext,
        event_type: &str,
        data: &Value,
        session_id: Option<&str>,
        created: Option<u64>,
    ) {
        let (Some(session_id), Some(msg_id), Some(call_id)) = (
            session_id,
            data.get("assistantMessageID")
                .or_else(|| data.get("messageID"))
                .and_then(Value::as_str),
            data.get("id").and_then(Value::as_str),
        ) else {
            tracing::debug!(event = event_type, "tool event without ids");
            return;
        };
        let asid = AgentSessionId(session_id.to_string());

        let patch = match event_type {
            // The name only ever arrives here; `session.tool.called` has none.
            "session.tool.input.started" => ToolPatch {
                name: data.get("name").and_then(Value::as_str).map(str::to_string),
                state: Some(ToolCallStatus::Pending),
                ..ToolPatch::default()
            },
            // `delta` is the chunk of argument text that just arrived -- the
            // schema in the 2.0.1 binary is `{sessionID, assistantMessageID,
            // id, delta}`, and OpenCode's own TUI and transcript builder both
            // concatenate it. Throwing it away left a pending card showing
            // nothing but the tool's name for as long as the arguments took.
            "session.tool.input.delta" => ToolPatch {
                input_delta: data
                    .get("delta")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                state: Some(ToolCallStatus::Streaming),
                ..ToolPatch::default()
            },
            "session.tool.input.ended" => {
                // `text` is the complete input as a JSON string.
                let input = data
                    .get("text")
                    .and_then(Value::as_str)
                    .and_then(|t| serde_json::from_str::<Value>(t).ok());
                ToolPatch {
                    input,
                    state: Some(ToolCallStatus::Streaming),
                    ..ToolPatch::default()
                }
            }
            "session.tool.called" => ToolPatch {
                input: data.get("input").cloned(),
                state: Some(ToolCallStatus::Running),
                ran_ms: created,
                ..ToolPatch::default()
            },
            "session.tool.progress" => ToolPatch {
                metadata: data.get("metadata").cloned(),
                state: Some(ToolCallStatus::Running),
                ..ToolPatch::default()
            },
            "session.tool.success" => {
                let content = data.get("content").cloned();
                ToolPatch {
                    output: content.as_ref().and_then(mapper::tool_content_to_output),
                    content,
                    metadata: data.get("metadata").cloned(),
                    state: Some(ToolCallStatus::Completed),
                    completed_ms: created,
                    ..ToolPatch::default()
                }
            }
            "session.tool.failed" => {
                let content = data.get("content").cloned();
                ToolPatch {
                    output: content.as_ref().and_then(mapper::tool_content_to_output),
                    content,
                    metadata: data.get("metadata").cloned(),
                    state: Some(ToolCallStatus::Failed),
                    error: data.get("error").and_then(mapper::map_error),
                    completed_ms: created,
                    ..ToolPatch::default()
                }
            }
            _ => return,
        };

        ctx.upsert_tool(&asid, msg_id, call_id, patch).await;
    }
}

mod deepseek;
mod t3;

#[cfg(test)]
mod tests;
