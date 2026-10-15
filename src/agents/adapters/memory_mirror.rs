use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agents::domain::{
    tool_item_id, AgentDomainEvent, AgentErrorInfo, AgentPart, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, FormRequest, FormResolutionReason, PermissionRequest, RevertState,
    SessionRevertInfo, TimelineItem, TimelineRole, ToolCall, ToolCallStatus, ToolTime,
};
use crate::agents::ports::mirror::{AgentSessionSnapshot, MirrorFuture, SessionMirrorPort};

/// Ring-buffer bounds. The mirror is a cache in front of OpenCode, not a store:
/// replay it drops is recovered with a snapshot. Authoritative text/reasoning
/// is not byte-capped: the current contract cannot report partial transcripts.
const MAX_EVENT_LOG_ENTRIES: usize = 2000;
/// Total serialized size of the per-session event log. A `TimelineUpsert` from
/// a refetch carries a whole timeline, so a count alone bounds nothing.
const MAX_EVENT_LOG_BYTES: usize = 4 * 1024 * 1024;
/// Timeline rows kept per session; the oldest are dropped first.
const MAX_TIMELINE_ITEMS: usize = 1500;
/// Per tool call, how much output text is kept. Beyond this the text is cut and
/// the part is flagged `truncated`.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
/// Sessions held in memory at once, and how long an untouched one survives.
const MAX_SESSIONS: usize = 200;
const SESSION_IDLE_EVICTION_MS: u64 = 6 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The stand-in used when an event arrives for a session the mirror has never
/// seen. It carries no title and no agent: a guessed `"Session"` title used to
/// be broadcast as if OpenCode had said it, and it outlived the real title on
/// the client. `placeholder` stays set until a real `Session.Info` lands.
fn placeholder_session(asid: &AgentSessionId, status: AgentSessionStatus) -> AgentSessionInfo {
    AgentSessionInfo {
        asid: asid.clone(),
        agent_id: String::new(),
        backend_session_id: asid.0.clone(),
        runtime: None,
        context_usage: None,
        git_info: None,
        agent_name: None,
        agent_role: None,
        title: String::new(),
        mode: None,
        model: None,
        status,
        directory: None,
        cost: None,
        tokens: None,
        limit: None,
        parent_id: None,
        project_id: None,
        outcome: None,
        error: None,
        revert: None,
        fork: None,
        time_idle: None,
        time_viewed: None,
        deleted: false,
        updated_ms: 0,
    }
}

/// Cut an oversized tool result rather than holding it forever. A single
/// `read` or `grep` result can be megabytes, and the mirror keeps every one.
fn truncate_tool_output(call: &mut ToolCall) {
    if let Some(serde_json::Value::String(ref mut text)) = call.output {
        if text.len() > MAX_TOOL_OUTPUT_BYTES {
            let mut cut = MAX_TOOL_OUTPUT_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n… output truncated by the gateway …");
            call.truncated = true;
        }
    }
    // The verbatim `content` array is the same payload a second time; the
    // gateway keeps it only while it is small.
    if let Some(ref content) = call.content {
        if serde_json::to_string(content).map(|s| s.len()).unwrap_or(0) > MAX_TOOL_OUTPUT_BYTES {
            call.content = None;
            call.truncated = true;
        }
    }
}

fn event_size(event: &AgentDomainEvent) -> usize {
    #[derive(Default)]
    struct ByteCount(usize);
    impl std::io::Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = ByteCount::default();
    match serde_json::to_writer(&mut count, event) {
        Ok(()) => count.0,
        // An unserializable event cannot be safely replayed.
        Err(_) => usize::MAX,
    }
}

struct ReplayEvent {
    event: AgentDomainEvent,
    bytes: usize,
}

struct SessionState {
    info: AgentSessionInfo,
    timeline: BTreeMap<String, TimelineItem>,
    permissions: HashMap<String, PermissionRequest>,
    forms: HashMap<String, FormRequest>,
    inbox: Vec<serde_json::Value>,
    event_log: VecDeque<ReplayEvent>,
    event_log_bytes: usize,
    /// Last discarded sequence, even when the replay log is now empty.
    replay_floor: u64,
    current_seq: u64,
    touched_ms: u64,
    /// True while `info` is the local stand-in rather than something OpenCode
    /// reported.
    placeholder: bool,
    /// Metadata and live deltas do not prove that initial history was read.
    history_initialized: bool,
    history_gate: Arc<tokio::sync::Mutex<()>>,
    history_removal_generation: u64,
}

impl SessionState {
    fn new(info: AgentSessionInfo) -> Self {
        Self {
            info,
            timeline: BTreeMap::new(),
            permissions: HashMap::new(),
            forms: HashMap::new(),
            inbox: Vec::new(),
            event_log: VecDeque::new(),
            event_log_bytes: 0,
            replay_floor: 0,
            current_seq: 0,
            touched_ms: now_ms(),
            placeholder: false,
            history_initialized: false,
            history_gate: Arc::new(tokio::sync::Mutex::new(())),
            history_removal_generation: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.current_seq += 1;
        self.touched_ms = now_ms();
        self.current_seq
    }

    fn push_event(&mut self, event: AgentDomainEvent) {
        let size = event_size(&event);
        if size > MAX_EVENT_LOG_BYTES {
            // Dropping only this event would leave a hole. Discard the entire
            // prefix through it; subsequent retained events form a suffix.
            self.event_log.clear();
            self.event_log_bytes = 0;
            self.replay_floor = self.replay_floor.max(event.seq());
            return;
        }
        self.event_log_bytes += size;
        self.event_log.push_back(ReplayEvent { event, bytes: size });
        while self.event_log.len() > MAX_EVENT_LOG_ENTRIES
            || self.event_log_bytes > MAX_EVENT_LOG_BYTES
        {
            if let Some(dropped) = self.event_log.pop_front() {
                self.event_log_bytes -= dropped.bytes;
                self.replay_floor = self.replay_floor.max(dropped.event.seq());
            } else {
                break;
            }
        }
    }

    /// Keep the timeline bounded by dropping the oldest rows. Order is
    /// `(message_id, ordinal)`, and message ids sort by creation, so the
    /// first keys are the oldest rows.
    fn trim_timeline(&mut self) {
        while self.timeline.len() > MAX_TIMELINE_ITEMS {
            let Some(oldest) = self
                .timeline
                .values()
                .min_by(|a, b| a.sort_key().cmp(&b.sort_key()))
                .map(|it| it.id.clone())
            else {
                break;
            };
            self.timeline.remove(&oldest);
        }
    }

    fn insert_item(&mut self, item: TimelineItem) {
        self.timeline.insert(item.id.clone(), item);
        self.trim_timeline();
    }

    fn ordered_timeline(&self) -> Vec<TimelineItem> {
        let mut items: Vec<TimelineItem> = self.timeline.values().cloned().collect();
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        items
    }

    /// The next free ordinal within a message, used for a streamed tool call --
    /// the tool events carry no ordinal of their own, and a refetch replaces
    /// this with the real content-array index.
    fn next_ordinal_for(&self, message_id: &str) -> u64 {
        self.timeline
            .values()
            .filter(|it| it.message_id == message_id)
            .map(|it| it.ordinal + 1)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Default)]
pub struct MemoryMirror {
    sessions: Arc<RwLock<HashMap<AgentSessionId, SessionState>>>,
    /// The owning agent's `kind()`, stamped on every session this mirror
    /// holds so snapshots and `agent.session.updated` events carry `agent_id`.
    agent_id: Arc<str>,
}

impl MemoryMirror {
    /// Return the exact bounded, revision-stamped event retained for replay.
    /// Live publishers must not resend the caller's unstamped input rows.
    pub(crate) async fn record_timeline_upsert(
        &self,
        asid: &AgentSessionId,
        items: Vec<TimelineItem>,
    ) -> AgentDomainEvent {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        let seq = state.next_seq();
        let mut updated_items = Vec::new();
        for mut item in items {
            item.seq = seq;
            if let AgentPart::Tool(ref mut call) = item.part {
                truncate_tool_output(call);
            }
            state.insert_item(item.clone());
            updated_items.push(item);
        }
        let event = AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: updated_items,
            seq,
        };
        state.push_event(event.clone());
        event
    }

    /// Preserve resolution semantics in the replay ring as well as live SSE.
    pub(crate) async fn resolve_form_with_reason(
        &self,
        asid: &AgentSessionId,
        form_id: &str,
        reason: Option<FormResolutionReason>,
    ) -> u64 {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        state.forms.remove(form_id);
        let seq = state.next_seq();
        state.push_event(AgentDomainEvent::FormResolved {
            asid: asid.clone(),
            form_id: form_id.into(),
            reason,
            seq,
        });
        seq
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Tag a session-carrying event with this mirror's agent, for events
    /// built from an agent read rather than from the mirror's own state.
    pub fn stamp_event(&self, event: &mut AgentDomainEvent) {
        if let AgentDomainEvent::SessionUpdated { info, .. } = event {
            if !self.agent_id.is_empty() {
                info.agent_id = self.agent_id.to_string();
            }
        }
    }

    /// A mirror for the sessions of one agent (the agent's `kind()`).
    pub fn for_agent(agent_id: &str) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            agent_id: Arc::from(agent_id),
        }
    }

    /// Drop sessions nothing has touched for a long time, and the oldest ones
    /// beyond the cap. Called whenever a new session is admitted.
    fn evict(sessions: &mut HashMap<AgentSessionId, SessionState>) {
        let now = now_ms();
        sessions.retain(|_, state| now.saturating_sub(state.touched_ms) < SESSION_IDLE_EVICTION_MS);
        while sessions.len() > MAX_SESSIONS {
            let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, state)| state.touched_ms)
                .map(|(asid, _)| asid.clone())
            else {
                break;
            };
            sessions.remove(&oldest);
        }
    }

    fn entry<'a>(
        agent_id: &str,
        sessions: &'a mut HashMap<AgentSessionId, SessionState>,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
    ) -> &'a mut SessionState {
        if !sessions.contains_key(asid) {
            let mut placeholder = placeholder_session(asid, status);
            placeholder.agent_id = agent_id.to_string();
            let mut fresh = SessionState::new(placeholder);
            fresh.placeholder = true;
            sessions.insert(asid.clone(), fresh);
            Self::evict(sessions);
        }
        sessions.get_mut(asid).expect("session was just inserted")
    }

    /// True when the cached `info` for this session is still the local
    /// stand-in, so the caller knows to fetch the real one.
    pub async fn is_placeholder(&self, asid: &AgentSessionId) -> bool {
        let sessions = self.sessions.read().await;
        sessions.get(asid).map(|s| s.placeholder).unwrap_or(true)
    }

    pub(crate) async fn history_gate(&self, asid: &AgentSessionId) -> Arc<tokio::sync::Mutex<()>> {
        let mut sessions = self.sessions.write().await;
        Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Unknown,
        )
        .history_gate
        .clone()
    }

    pub(crate) async fn history_initialized(&self, asid: &AgentSessionId) -> bool {
        self.sessions
            .read()
            .await
            .get(asid)
            .is_some_and(|state| state.history_initialized)
    }

    pub(crate) async fn history_read_revision(&self, asid: &AgentSessionId) -> Option<(u64, u64)> {
        self.sessions
            .read()
            .await
            .get(asid)
            .map(|state| (state.current_seq, state.history_removal_generation))
    }

    /// A newly created session has known empty history. Metadata for an existing
    /// session or a fork does not establish that fact.
    pub(crate) async fn mark_history_initialized(&self, asid: &AgentSessionId) {
        if let Some(state) = self.sessions.write().await.get_mut(asid) {
            state.history_initialized = true;
        }
    }

    /// Merge one bounded initial history read without replacing newer live rows.
    /// The gate identity fences eviction/recreation while the provider was read.
    #[cfg(test)]
    pub(crate) async fn initialize_history(
        &self,
        asid: &AgentSessionId,
        gate: &Arc<tokio::sync::Mutex<()>>,
        before_read: (u64, u64),
        items: Vec<TimelineItem>,
    ) -> bool {
        self.initialize_prepared_history(asid, gate, before_read, items, None)
            .await
            .expect("stateless history acceptance cannot fail")
    }

    pub(crate) async fn initialize_prepared_history(
        &self,
        asid: &AgentSessionId,
        gate: &Arc<tokio::sync::Mutex<()>>,
        before_read: (u64, u64),
        items: Vec<TimelineItem>,
        commit: Option<crate::agents::ports::agent::TimelineCommit<'_>>,
    ) -> Result<bool, crate::agents::ports::agent::AgentError> {
        let mut sessions = self.sessions.write().await;
        let Some(state) = sessions.get_mut(asid) else {
            return Ok(false);
        };
        if !Arc::ptr_eq(gate, &state.history_gate) {
            return Ok(false);
        }
        if state.history_initialized {
            return Ok(true);
        }
        if state.history_removal_generation != before_read.1 {
            return Ok(false);
        }
        // Both fences have passed. The adapter's one-shot synchronous commit
        // and mirror acceptance share this write lock and one uncancellable poll.
        if let Some(commit) = commit {
            commit()?;
        }
        let mut accepted: Vec<_> = items
            .into_iter()
            .filter(|item| {
                state
                    .timeline
                    .get(&item.id)
                    .is_none_or(|current| current.seq <= before_read.0)
            })
            .collect();
        if !accepted.is_empty() {
            let seq = state.next_seq();
            for item in &mut accepted {
                item.seq = seq;
                if let AgentPart::Tool(call) = &mut item.part {
                    truncate_tool_output(call);
                }
                state.insert_item(item.clone());
            }
            state.push_event(AgentDomainEvent::TimelineUpsert {
                asid: asid.clone(),
                items: accepted,
                seq,
            });
        }
        state.history_initialized = true;
        Ok(true)
    }

    /// Upstream metadata cannot erase read markers the Gateway owns.
    pub async fn overlay_read_markers(&self, info: &mut AgentSessionInfo) {
        let sessions = self.sessions.read().await;
        if let Some(state) = sessions.get(&info.asid) {
            info.time_idle = info.time_idle.max(state.info.time_idle);
            info.time_viewed = info.time_viewed.max(state.info.time_viewed);
        }
    }

    /// Reading a session is Gateway-owned when the upstream has no read marker.
    pub async fn mark_viewed(
        &self,
        asid: &AgentSessionId,
        viewed: u64,
    ) -> Option<(u64, AgentSessionInfo)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        state.info.time_viewed = Some(viewed.max(state.info.time_viewed.unwrap_or(0)));
        let seq = state.next_seq();
        let info = state.info.clone();
        state.push_event(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info.clone()),
            seq,
        });
        Some((seq, info))
    }

    pub async fn update_status(
        &self,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
        error: Option<AgentErrorInfo>,
    ) -> Option<u64> {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        let previous_status = state.info.status;
        state.info.status = status;
        if error.is_some() {
            state.info.error = error.clone();
        } else if status != AgentSessionStatus::Failed {
            state.info.error = None;
        }
        if status == AgentSessionStatus::Idle && previous_status != AgentSessionStatus::Idle {
            state.info.time_idle = Some(now_ms());
        }
        let seq = state.next_seq();
        // A status change is logged as a status change: replaying it as a
        // `SessionUpdated` republished whatever `info` happened to be cached,
        // placeholder included.
        let event = AgentDomainEvent::StatusChanged {
            asid: asid.clone(),
            status,
            error,
            seq,
        };
        state.push_event(event);
        Some(seq)
    }

    /// Merge what an event told us about a session into the cached info,
    /// leaving every field the event did not mention alone. A title OpenCode
    /// has given is never replaced by an empty one.
    pub async fn merge_session_fields(
        &self,
        asid: &AgentSessionId,
        patch: SessionPatch,
    ) -> Option<(u64, AgentSessionInfo)> {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );

        if let Some(title) = patch.title {
            if !title.trim().is_empty() {
                state.info.title = title;
                state.placeholder = false;
            }
        }
        if let Some(agent) = patch.agent {
            state.info.mode = Some(agent);
        }
        if let Some(model) = patch.model {
            state.info.model = Some(model);
        }
        if let Some(parent_id) = patch.parent_id {
            state.info.parent_id = Some(parent_id);
        }
        if let Some(directory) = patch.directory {
            state.info.directory = Some(directory);
        }
        if let Some(project_id) = patch.project_id {
            state.info.project_id = Some(project_id);
        }
        if let Some(outcome) = patch.outcome {
            state.info.outcome = Some(outcome);
        }
        if patch.deleted {
            state.info.deleted = true;
        }
        state.info.updated_ms = now_ms();

        let seq = state.next_seq();
        let info = state.info.clone();
        let event = AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info.clone()),
            seq,
        };
        state.push_event(event);
        Some((seq, info))
    }

    /// Forget a session OpenCode has deleted.
    pub async fn remove_session(&self, asid: &AgentSessionId) {
        let mut sessions = self.sessions.write().await;
        sessions.remove(asid);
    }

    pub async fn set_inbox(
        &self,
        asid: &AgentSessionId,
        items: Vec<serde_json::Value>,
    ) -> (u64, Vec<serde_json::Value>) {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        state.inbox = items;
        let seq = state.next_seq();
        let items = state.inbox.clone();
        let event = AgentDomainEvent::InboxChanged {
            asid: asid.clone(),
            items: items.clone(),
            seq,
        };
        state.push_event(event);
        (seq, items)
    }

    pub async fn inbox(&self, asid: &AgentSessionId) -> Vec<serde_json::Value> {
        let sessions = self.sessions.read().await;
        sessions
            .get(asid)
            .map(|s| s.inbox.clone())
            .unwrap_or_default()
    }

    /// A delivery change carries no payload: mutate a known item in place.
    /// `None` asks the caller to fetch the authoritative queue after releasing
    /// the mirror lock, rather than inventing an item.
    pub async fn update_inbox_delivery(
        &self,
        asid: &AgentSessionId,
        inbox_id: &str,
        delivery: &str,
    ) -> Option<(u64, Vec<serde_json::Value>)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        let item = state
            .inbox
            .iter_mut()
            .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(inbox_id))?;
        item.as_object_mut()?
            .insert("delivery".to_string(), serde_json::json!(delivery));
        let seq = state.next_seq();
        let items = state.inbox.clone();
        state.push_event(AgentDomainEvent::InboxChanged {
            asid: asid.clone(),
            items: items.clone(),
            seq,
        });
        Some((seq, items))
    }

    /// Record an inbox item the stream announced, without a round trip.
    pub async fn upsert_inbox_item(
        &self,
        asid: &AgentSessionId,
        inbox_id: &str,
        item: Option<serde_json::Value>,
    ) -> (u64, Vec<serde_json::Value>) {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        state
            .inbox
            .retain(|it| it.get("id").and_then(serde_json::Value::as_str) != Some(inbox_id));
        if let Some(mut item) = item {
            if item.get("id").is_none() {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("id".to_string(), serde_json::json!(inbox_id));
                }
            }
            state.inbox.push(item);
        }
        let seq = state.next_seq();
        let items = state.inbox.clone();
        let event = AgentDomainEvent::InboxChanged {
            asid: asid.clone(),
            items: items.clone(),
            seq,
        };
        state.push_event(event);
        (seq, items)
    }

    /// Record where a rollback stands and publish it as
    /// `agent.revert.changed`. `info.revert` is set in the same call, so a
    /// snapshot served out of the mirror agrees with what went down the
    /// stream -- the mirror is what answers `GET /api/agent-sessions/{asid}`
    /// for a session it already holds.
    pub async fn record_revert(
        &self,
        asid: &AgentSessionId,
        revert_state: RevertState,
        revert: Option<SessionRevertInfo>,
    ) -> (u64, AgentSessionInfo) {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        state.info.revert = revert.clone();
        state.info.updated_ms = now_ms();
        let seq = state.next_seq();
        let event = AgentDomainEvent::RevertChanged {
            asid: asid.clone(),
            state: revert_state,
            revert,
            seq,
        };
        state.push_event(event);
        (seq, state.info.clone())
    }

    /// Drop the rows a committed rollback took with it.
    ///
    /// OpenCode deletes the boundary message and everything after it and says
    /// nothing further -- there is no message-removed event in 2.0.1 -- so
    /// without this the mirror keeps serving rows that no longer exist. Ids
    /// sort by creation, which is what makes "at or after the boundary" a
    /// range; rows that are not messages (a detached shell is keyed
    /// `shell_…`) are left alone.
    pub async fn remove_timeline_from(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
    ) -> Option<(u64, Vec<String>)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        // An unloaded row may still be in an in-flight canonical read.
        state.history_removal_generation = state.history_removal_generation.wrapping_add(1);
        let doomed: Vec<String> = state
            .timeline
            .values()
            .filter(|item| {
                item.message_id.starts_with("msg_") && item.message_id.as_str() >= message_id
            })
            .map(|item| item.id.clone())
            .collect();
        if doomed.is_empty() {
            return None;
        }
        for id in &doomed {
            state.timeline.remove(id);
        }
        let seq = state.next_seq();
        let event = AgentDomainEvent::TimelineRemoved {
            asid: asid.clone(),
            ids: doomed.clone(),
            seq,
        };
        state.push_event(event);
        Some((seq, doomed))
    }

    /// Drop exactly the rows named. What a streamed assistant attempt leaves
    /// behind once its committed message has been read under its real id.
    pub async fn remove_timeline_items(
        &self,
        asid: &AgentSessionId,
        ids: &[String],
    ) -> Option<(u64, Vec<String>)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        // An unloaded row may still be in an in-flight canonical read.
        state.history_removal_generation = state.history_removal_generation.wrapping_add(1);
        let removed: Vec<String> = ids
            .iter()
            .filter(|id| state.timeline.remove(id.as_str()).is_some())
            .cloned()
            .collect();
        if removed.is_empty() {
            return None;
        }
        let seq = state.next_seq();
        let event = AgentDomainEvent::TimelineRemoved {
            asid: asid.clone(),
            ids: removed.clone(),
            seq,
        };
        state.push_event(event);
        Some((seq, removed))
    }

    pub async fn record_compaction(
        &self,
        asid: &AgentSessionId,
        status: crate::agents::domain::CompactionStatus,
        reason: Option<String>,
        delta: Option<String>,
    ) -> u64 {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Busy,
        );
        let seq = state.next_seq();
        let event = AgentDomainEvent::CompactionChanged {
            asid: asid.clone(),
            status,
            reason,
            delta,
            seq,
        };
        state.push_event(event);
        seq
    }

    /// Direct helper to append streaming text/reasoning chunk to existing timeline item
    pub async fn append_text_delta(
        &self,
        asid: &AgentSessionId,
        item_id: &str,
        message_id: &str,
        ordinal: u64,
        delta: &str,
        is_reasoning: bool,
    ) -> Option<u64> {
        self.write_text(
            asid,
            item_id,
            message_id,
            ordinal,
            delta,
            is_reasoning,
            true,
        )
        .await
    }

    pub async fn set_text_content(
        &self,
        asid: &AgentSessionId,
        item_id: &str,
        message_id: &str,
        ordinal: u64,
        full_text: &str,
        is_reasoning: bool,
    ) -> Option<u64> {
        self.write_text(
            asid,
            item_id,
            message_id,
            ordinal,
            full_text,
            is_reasoning,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_text(
        &self,
        asid: &AgentSessionId,
        item_id: &str,
        message_id: &str,
        ordinal: u64,
        text: &str,
        is_reasoning: bool,
        append: bool,
    ) -> Option<u64> {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Busy,
        );

        let now = now_ms();
        let seq = state.next_seq();

        let item = if let Some(item) = state.timeline.get_mut(item_id) {
            match &mut item.part {
                AgentPart::Text { text: existing } if !is_reasoning => {
                    if append {
                        existing.push_str(text);
                    } else {
                        *existing = text.to_string();
                    }
                }
                AgentPart::Reasoning { text: existing, .. } if is_reasoning => {
                    if append {
                        existing.push_str(text);
                    } else {
                        *existing = text.to_string();
                    }
                }
                _ => return None,
            }
            item.updated_ms = now;
            item.seq = seq;
            item.clone()
        } else {
            let part = if is_reasoning {
                AgentPart::Reasoning {
                    text: text.to_string(),
                    duration_ms: None,
                }
            } else {
                AgentPart::Text {
                    text: text.to_string(),
                }
            };
            let item = TimelineItem {
                id: item_id.to_string(),
                message_id: message_id.to_string(),
                seq,
                updated_ms: now,
                ordinal,
                role: TimelineRole::Assistant,
                part,
                attachments: None,
                image_assets: None,
            };
            state.insert_item(item.clone());
            item
        };

        let event = AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: vec![item],
            seq,
        };
        state.push_event(event);
        Some(seq)
    }

    /// Create or update the tool card for one call id. The `session.tool.*`
    /// events each carry a slice of the card -- name, then input, then progress
    /// metadata, then the result -- so they are joined here rather than waiting
    /// for a refetch of the whole message.
    pub async fn upsert_tool_call(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
        tool_call_id: &str,
        patch: ToolPatch,
    ) -> Option<(u64, TimelineItem)> {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Busy,
        );

        let item_id = tool_item_id(message_id, tool_call_id);
        let now = now_ms();
        let seq = state.next_seq();
        let ordinal = state.next_ordinal_for(message_id);

        let existing = state.timeline.get_mut(&item_id);
        let item = match existing {
            Some(item) => {
                let AgentPart::Tool(ref mut call) = item.part else {
                    return None;
                };
                patch.apply(call);
                truncate_tool_output(call);
                item.updated_ms = now;
                item.seq = seq;
                item.clone()
            }
            None => {
                let mut call = ToolCall {
                    id: tool_call_id.to_string(),
                    name: patch.name.clone().unwrap_or_else(|| "tool".to_string()),
                    title: None,
                    input: serde_json::Value::Null,
                    output: None,
                    content: None,
                    metadata: None,
                    state: ToolCallStatus::Pending,
                    status: ToolCallStatus::Pending,
                    error: None,
                    child_session_id: None,
                    background: false,
                    input_partial: None,
                    truncated: false,
                    time: ToolTime {
                        created: Some(now),
                        ran: None,
                        completed: None,
                    },
                };
                patch.apply(&mut call);
                truncate_tool_output(&mut call);
                let item = TimelineItem {
                    id: item_id.clone(),
                    message_id: message_id.to_string(),
                    role: TimelineRole::Assistant,
                    part: AgentPart::Tool(call),
                    seq,
                    updated_ms: now,
                    ordinal,
                    attachments: None,
                    image_assets: None,
                };
                state.insert_item(item.clone());
                item
            }
        };

        let event = AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: vec![item.clone()],
            seq,
        };
        state.push_event(event);
        Some((seq, item))
    }

    /// Mark every still-running tool in a session as backgrounded, which is
    /// what `POST .../background` does to them.
    pub async fn mark_running_tools_backgrounded(&self, asid: &AgentSessionId) -> Option<u64> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        let mut changed = Vec::new();
        for item in state.timeline.values_mut() {
            if let AgentPart::Tool(ref mut call) = item.part {
                if matches!(
                    call.state,
                    ToolCallStatus::Running | ToolCallStatus::Pending | ToolCallStatus::Streaming
                ) && !call.background
                {
                    call.background = true;
                    changed.push(item.clone());
                }
            }
        }
        if changed.is_empty() {
            return None;
        }
        let seq = state.next_seq();
        for item in changed.iter_mut() {
            item.seq = seq;
            state.timeline.insert(item.id.clone(), item.clone());
        }
        let event = AgentDomainEvent::TimelineUpsert {
            asid: asid.clone(),
            items: changed,
            seq,
        };
        state.push_event(event);
        Some(seq)
    }

    pub async fn get_timeline_delta(
        &self,
        asid: &AgentSessionId,
        after_seq: u64,
    ) -> (Vec<TimelineItem>, Option<AgentSessionStatus>, bool, u64) {
        let sessions = self.sessions.read().await;
        let Some(state) = sessions.get(asid) else {
            return (Vec::new(), None, false, 0);
        };

        let current_seq = state.current_seq;
        let status = Some(state.info.status);

        if after_seq < state.replay_floor {
            return (Vec::new(), status, true, current_seq);
        }

        let mut items: Vec<TimelineItem> = state
            .timeline
            .values()
            .filter(|it| it.seq > after_seq)
            .cloned()
            .collect();
        items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        (items, status, false, current_seq)
    }

    /// The session's directory as the mirror last heard it, without cloning
    /// anything else.
    pub async fn session_directory(&self, asid: &str) -> Option<String> {
        let sessions = self.sessions.read().await;
        sessions
            .get(&AgentSessionId(asid.to_owned()))?
            .info
            .directory
            .clone()
    }

    pub async fn get_timeline_item(
        &self,
        asid: &AgentSessionId,
        item_id: &str,
    ) -> Option<TimelineItem> {
        let sessions = self.sessions.read().await;
        sessions.get(asid)?.timeline.get(item_id).cloned()
    }

    pub async fn update_usage(
        &self,
        asid: &AgentSessionId,
        cost: Option<f64>,
        tokens: Option<crate::agents::domain::TokensUsage>,
    ) -> Option<(u64, AgentSessionInfo)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        if cost.is_some() {
            state.info.cost = cost;
        }
        if tokens.is_some() {
            state.info.tokens = tokens;
        }
        let seq = state.next_seq();
        let event = AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(state.info.clone()),
            seq,
        };
        state.push_event(event);
        Some((seq, state.info.clone()))
    }

    /// Replace the pending permission and form sets wholesale, which is what a
    /// snapshot catch-up produces.
    pub async fn replace_pending(
        &self,
        asid: &AgentSessionId,
        permissions: Vec<PermissionRequest>,
        forms: Vec<FormRequest>,
    ) {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(
            &self.agent_id,
            &mut sessions,
            asid,
            AgentSessionStatus::Idle,
        );
        state.permissions = permissions.into_iter().map(|p| (p.id.clone(), p)).collect();
        state.forms = forms.into_iter().map(|f| (f.id.clone(), f)).collect();
    }

    #[cfg(test)]
    pub async fn timeline_len(&self, asid: &AgentSessionId) -> usize {
        let sessions = self.sessions.read().await;
        sessions.get(asid).map(|s| s.timeline.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub async fn event_log_len(&self, asid: &AgentSessionId) -> usize {
        let sessions = self.sessions.read().await;
        sessions.get(asid).map(|s| s.event_log.len()).unwrap_or(0)
    }

    /// Bounded by this mirror's normal session retention. Captured sequences
    /// fence runtime observations that raced newer native events.
    pub(crate) async fn runtime_sessions(&self) -> Vec<(AgentSessionId, u64)> {
        self.sessions
            .read()
            .await
            .iter()
            .filter(|(_, state)| !state.placeholder)
            .map(|(id, state)| (id.clone(), state.current_seq))
            .collect()
    }

    /// Metadata-only fence: native token events must not clone retained history.
    pub(crate) async fn non_native_runtime_seq(&self, asid: &AgentSessionId) -> Option<u64> {
        let sessions = self.sessions.read().await;
        let state = sessions.get(asid)?;
        state
            .info
            .runtime
            .as_ref()
            .filter(|runtime| runtime.source != crate::agents::domain::RuntimeSource::Native)
            .map(|_| state.current_seq)
    }

    pub(crate) async fn update_runtime(
        &self,
        asid: &AgentSessionId,
        expected: u64,
        runtime: crate::agents::domain::SessionRuntime,
    ) -> Option<(u64, AgentSessionInfo)> {
        self.update_runtime_decorated(asid, expected, runtime, |_| {})
            .await
    }

    pub(crate) async fn update_runtime_decorated(
        &self,
        asid: &AgentSessionId,
        expected: u64,
        runtime: crate::agents::domain::SessionRuntime,
        decorate: impl FnOnce(&mut AgentSessionInfo),
    ) -> Option<(u64, AgentSessionInfo)> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        if state.current_seq != expected {
            return None;
        }
        let previous = state.info.clone();
        runtime.apply(&mut state.info);
        decorate(&mut state.info);
        if state.info == previous {
            return None;
        }
        let seq = state.next_seq();
        let info = state.info.clone();
        state.push_event(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info.clone()),
            seq,
        });
        Some((seq, info))
    }

    pub(crate) async fn observation_epoch(&self, asid: &AgentSessionId) -> Option<u64> {
        self.sessions
            .read()
            .await
            .get(asid)
            .map(|state| state.replay_floor)
    }

    /// Atomically order observation metadata before appended items. Explicit
    /// snapshot replacement and native ownership changes fence older batches.
    pub(crate) async fn apply_observation(
        &self,
        asid: &AgentSessionId,
        epoch: u64,
        runtime: crate::agents::domain::SessionRuntime,
        items: Vec<TimelineItem>,
        decorate: impl FnOnce(&mut AgentSessionInfo),
        publish: impl FnOnce(&[AgentDomainEvent]),
    ) -> Option<Vec<AgentDomainEvent>> {
        use crate::agents::domain::RuntimeSource;
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;
        if state.replay_floor != epoch
            || state.placeholder
            || state.info.runtime.as_ref().is_none_or(|old| {
                old.source != RuntimeSource::Terminal || old.terminal != runtime.terminal
            })
            || runtime.source != RuntimeSource::Terminal
        {
            return None;
        }
        let previous = state.info.clone();
        runtime.apply(&mut state.info);
        decorate(&mut state.info);
        let mut events = Vec::new();
        if previous != state.info {
            let seq = state.next_seq();
            events.push(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(state.info.clone()),
                seq,
            });
        }
        if !items.is_empty() {
            let seq = state.next_seq();
            let mut updated = Vec::new();
            for mut item in items {
                item.seq = seq;
                if let AgentPart::Tool(ref mut call) = item.part {
                    truncate_tool_output(call);
                }
                state.insert_item(item.clone());
                updated.push(item);
            }
            events.push(AgentDomainEvent::TimelineUpsert {
                asid: asid.clone(),
                items: updated,
                seq,
            });
        }
        for event in &events {
            state.push_event(event.clone());
        }
        // No I/O/await in publication. Later mirror writes cannot overtake this
        // batch's metadata and items on the provider's broadcast stream.
        publish(&events);
        Some(events)
    }

    /// Commit an explicitly requested provider read only if no newer event won.
    /// Invalidate older replay cursors; never broadcast replacement transcript rows.
    #[cfg(test)]
    pub(crate) async fn refresh_snapshot(
        &self,
        snapshot: crate::agents::ports::mirror::AgentSessionSnapshot,
        expected: Option<u64>,
    ) -> Option<crate::agents::ports::mirror::AgentSessionSnapshot> {
        self.refresh_prepared_snapshot(snapshot, expected, None, None)
            .await
            .expect("stateless snapshot acceptance cannot fail")
    }

    /// Capture sequence and incarnation together, fencing same-seq recreation.
    pub(crate) async fn snapshot_read_revision(
        &self,
        asid: &AgentSessionId,
    ) -> (Option<u64>, Option<Arc<tokio::sync::Mutex<()>>>) {
        let sessions = self.sessions.read().await;
        match sessions.get(asid) {
            Some(state) => (Some(state.current_seq), Some(state.history_gate.clone())),
            None => (None, None),
        }
    }

    pub(crate) async fn refresh_prepared_snapshot(
        &self,
        mut snapshot: crate::agents::ports::mirror::AgentSessionSnapshot,
        expected: Option<u64>,
        incarnation: Option<Arc<tokio::sync::Mutex<()>>>,
        commit: Option<crate::agents::ports::agent::TimelineCommit<'_>>,
    ) -> Result<
        Option<crate::agents::ports::mirror::AgentSessionSnapshot>,
        crate::agents::ports::agent::AgentError,
    > {
        let mut sessions = self.sessions.write().await;
        let asid = snapshot.info.asid.clone();
        if sessions.get(&asid).map(|s| s.current_seq) != expected
            || incarnation.as_ref().is_some_and(|gate| {
                sessions
                    .get(&asid)
                    .is_none_or(|state| !Arc::ptr_eq(gate, &state.history_gate))
            })
        {
            return Ok(None);
        }
        // No await after this commit: a rejected adapter token cannot alter the
        // mirror, and cancellation cannot split pagination from accepted rows.
        if let Some(commit) = commit {
            commit()?;
        }
        if let Some(previous) = sessions.get(&asid) {
            snapshot.info.time_viewed = previous.info.time_viewed;
            snapshot.info.time_idle = previous.info.time_idle;
        }
        let state = Self::entry(&self.agent_id, &mut sessions, &asid, snapshot.info.status);
        state.info = snapshot.info.clone();
        state.placeholder = false;
        state.history_initialized = true;
        state.timeline.clear();
        for item in &snapshot.timeline {
            state.insert_item(item.clone());
        }
        state.permissions = snapshot
            .permissions
            .iter()
            .map(|p| (p.id.clone(), p.clone()))
            .collect();
        state.forms = snapshot
            .forms
            .iter()
            .map(|p| (p.id.clone(), p.clone()))
            .collect();
        snapshot.inbox = state.inbox.clone();
        snapshot.seq = state.next_seq();
        state.event_log.clear();
        state.event_log_bytes = 0;
        state.replay_floor = snapshot.seq;
        Ok(Some(snapshot))
    }

    /// The session's info as held, without the rest of its snapshot.
    pub async fn session_info(&self, asid: &AgentSessionId) -> Option<AgentSessionInfo> {
        let sessions = self.sessions.read().await;
        sessions.get(asid).map(|state| state.info.clone())
    }

    #[cfg(test)]
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// The fields one session event reported. Everything left `None` keeps the
/// value the mirror already had.
#[derive(Debug, Default, Clone)]
pub struct SessionPatch {
    pub title: Option<String>,
    pub agent: Option<String>,
    pub model: Option<crate::agents::domain::ModelRef>,
    pub parent_id: Option<String>,
    pub directory: Option<String>,
    pub project_id: Option<String>,
    pub outcome: Option<String>,
    pub deleted: bool,
}

/// The slice of a tool card one `session.tool.*` event carried.
#[derive(Debug, Default, Clone)]
pub struct ToolPatch {
    pub name: Option<String>,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub content: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
    pub state: Option<ToolCallStatus>,
    /// One `session.tool.input.delta` chunk, appended to the call's
    /// `input_partial` rather than replacing it -- the event carries the piece
    /// that just arrived, not the whole of it so far.
    pub input_delta: Option<String>,
    pub error: Option<AgentErrorInfo>,
    pub ran_ms: Option<u64>,
    pub completed_ms: Option<u64>,
}

impl ToolPatch {
    fn apply(&self, call: &mut ToolCall) {
        if let Some(ref name) = self.name {
            call.name = name.clone();
        }
        if let Some(ref delta) = self.input_delta {
            crate::agents::domain::push_input_partial(
                call.input_partial.get_or_insert_with(String::new),
                delta,
            );
        }
        if let Some(ref input) = self.input {
            call.input = input.clone();
            call.title = crate::agents::adapters::opencode::mapper::tool_title(&call.name, input);
            // The real input is here; the preview it was standing in for has
            // nothing left to say.
            call.input_partial = None;
        }
        if let Some(ref output) = self.output {
            call.output = Some(output.clone());
        }
        if let Some(ref content) = self.content {
            call.content = Some(content.clone());
        }
        if let Some(ref metadata) = self.metadata {
            call.metadata = Some(metadata.clone());
        }
        if let Some(state) = self.state {
            // A finished call stays finished. `session.tool.progress` and
            // `session.tool.called` both say "running", and OpenCode does not
            // promise they reach us before that call's `success`: when a model
            // fires several tools at once -- nine parallel `read`s -- a late
            // progress event for one of them arrives after it has completed,
            // and nothing ever follows it. The mirror then held a completed
            // call as `running` for the rest of the session, and the app drew
            // a spinner beside a file that had been read minutes ago. A
            // terminal state is only replaced by another terminal state.
            let finished = matches!(
                call.state,
                ToolCallStatus::Completed | ToolCallStatus::Failed
            );
            let finishes = matches!(state, ToolCallStatus::Completed | ToolCallStatus::Failed);
            if !finished || finishes {
                call.set_state(state);
            }
        }
        if let Some(ref error) = self.error {
            call.error = Some(error.clone());
        }
        if let Some(ran) = self.ran_ms {
            call.time.ran = Some(ran);
        }
        if let Some(completed) = self.completed_ms {
            call.time.completed = Some(completed);
        }
        crate::agents::adapters::opencode::mapper::apply_tool_metadata(call);
    }
}

impl SessionMirrorPort for MemoryMirror {
    fn get_snapshot<'a>(
        &'a self,
        asid: &'a AgentSessionId,
    ) -> MirrorFuture<'a, Option<AgentSessionSnapshot>> {
        Box::pin(async move {
            let sessions = self.sessions.read().await;
            let state = sessions.get(asid)?;

            Some(AgentSessionSnapshot {
                info: state.info.clone(),
                timeline: state.ordered_timeline(),
                permissions: state.permissions.values().cloned().collect(),
                forms: state.forms.values().cloned().collect(),
                inbox: state.inbox.clone(),
                seq: state.current_seq,
            })
        })
    }

    fn get_events_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after_seq: u64,
    ) -> MirrorFuture<'a, Option<Vec<AgentDomainEvent>>> {
        Box::pin(async move {
            let sessions = self.sessions.read().await;
            let state = sessions.get(asid)?;

            if after_seq < state.replay_floor {
                return None;
            }

            let events: Vec<AgentDomainEvent> = state
                .event_log
                .iter()
                .filter(|ev| ev.event.seq() > after_seq)
                .map(|ev| ev.event.clone())
                .collect();
            Some(events)
        })
    }

    fn update_session<'a>(&'a self, info: AgentSessionInfo) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let mut info = info;
            if !self.agent_id.is_empty() {
                info.agent_id = self.agent_id.to_string();
            }
            let asid = info.asid.clone();
            if !sessions.contains_key(&asid) {
                sessions.insert(asid.clone(), SessionState::new(info.clone()));
                Self::evict(&mut sessions);
            }
            let Some(state) = sessions.get_mut(&asid) else {
                return 0;
            };
            // A read of `Session.Info` says nothing about a run in progress, so
            // a live status is not thrown away by a refetch.
            let live_status = state.info.status;
            let mut merged = info.clone();
            if merged.status == AgentSessionStatus::Idle
                && matches!(
                    live_status,
                    AgentSessionStatus::Busy | AgentSessionStatus::Retry
                )
            {
                merged.status = live_status;
            }
            // Nor does a read that says the session failed know why; the
            // reason the failure event carried is kept.
            if merged.status == AgentSessionStatus::Failed
                && merged.error.is_none()
                && state.info.status == AgentSessionStatus::Failed
            {
                merged.error = state.info.error.clone();
            }
            merged.time_idle = merged.time_idle.max(state.info.time_idle);
            merged.time_viewed = merged.time_viewed.max(state.info.time_viewed);
            state.info = merged.clone();
            state.placeholder = false;
            let seq = state.next_seq();

            let event = AgentDomainEvent::SessionUpdated {
                asid,
                info: Box::new(merged),
                seq,
            };
            state.push_event(event);
            seq
        })
    }

    fn upsert_timeline_items<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        items: Vec<TimelineItem>,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(async move { self.record_timeline_upsert(asid, items).await.seq() })
    }

    fn add_permission<'a>(&'a self, request: PermissionRequest) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = request.asid.clone();
            // A permission raised on a session the mirror has not otherwise
            // seen used to be dropped on the floor and answered with `seq: 0`.
            let state = Self::entry(
                &self.agent_id,
                &mut sessions,
                &asid,
                AgentSessionStatus::Busy,
            );
            let seq = state.next_seq();
            state
                .permissions
                .insert(request.id.clone(), request.clone());

            let event = AgentDomainEvent::PermissionPending { asid, request, seq };
            state.push_event(event);
            seq
        })
    }

    fn resolve_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let state = Self::entry(
                &self.agent_id,
                &mut sessions,
                asid,
                AgentSessionStatus::Idle,
            );
            state.permissions.remove(request_id);
            let seq = state.next_seq();

            let event = AgentDomainEvent::PermissionResolved {
                asid: asid.clone(),
                request_id: request_id.to_string(),
                seq,
            };
            state.push_event(event);
            seq
        })
    }

    fn add_form<'a>(&'a self, request: FormRequest) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = request.asid.clone();
            let state = Self::entry(
                &self.agent_id,
                &mut sessions,
                &asid,
                AgentSessionStatus::Busy,
            );
            let seq = state.next_seq();
            state.forms.insert(request.id.clone(), request.clone());

            let event = AgentDomainEvent::FormPending { asid, request, seq };
            state.push_event(event);
            seq
        })
    }

    fn resolve_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(self.resolve_form_with_reason(asid, form_id, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, AgentSessionInfo, AgentSessionStatus, TimelineRole};

    fn info(asid: &AgentSessionId, title: &str) -> AgentSessionInfo {
        let mut info = placeholder_session(asid, AgentSessionStatus::Idle);
        info.title = title.to_string();
        info.mode = Some("build".to_string());
        info.updated_ms = 1000;
        info
    }

    fn text_item(id: &str, message_id: &str, text: &str) -> TimelineItem {
        TimelineItem {
            id: id.to_string(),
            message_id: message_id.to_string(),
            role: TimelineRole::User,
            part: AgentPart::Text {
                text: text.to_string(),
            },
            seq: 0,
            updated_ms: 1001,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        }
    }

    fn replay_text(seq: u64, text: &str) -> AgentDomainEvent {
        AgentDomainEvent::TimelineUpsert {
            asid: AgentSessionId("s".into()),
            items: vec![text_item("m:t0", "m", text)],
            seq,
        }
    }

    #[tokio::test]
    async fn persisted_batch_cannot_cross_explicit_refresh_or_native_ownership() {
        use crate::agents::domain::{
            RuntimeInteraction, RuntimeSource, RuntimeTerminal, SessionRuntime, TranscriptSource,
        };
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("fixture".into());
        let runtime = SessionRuntime {
            source: RuntimeSource::Terminal,
            interaction: RuntimeInteraction::Terminal,
            transcript: Some(TranscriptSource::Persisted),
            status: None,
            reason: None,
            terminal: Some(RuntimeTerminal {
                session_id: "local".into(),
                pane_id: "pane".into(),
                instance_id: "binding".into(),
            }),
        };
        let mut session = info(&asid, "fixture");
        runtime.clone().apply(&mut session);
        mirror.update_session(session).await;
        let old_epoch = mirror.observation_epoch(&asid).await.unwrap();
        let captured = mirror.get_snapshot(&asid).await.unwrap();
        mirror
            .refresh_snapshot(captured.clone(), Some(captured.seq))
            .await
            .unwrap();
        assert!(mirror
            .apply_observation(
                &asid,
                old_epoch,
                runtime.clone(),
                vec![text_item("stale", "stale", "stale")],
                |_| {},
                |_| {}
            )
            .await
            .is_none());
        let epoch = mirror.observation_epoch(&asid).await.unwrap();
        let events = mirror
            .apply_observation(
                &asid,
                epoch,
                runtime.clone(),
                vec![text_item("current", "current", "current")],
                |info| info.title = "observed".into(),
                |_| {},
            )
            .await
            .unwrap();
        assert!(matches!(
            &events[0],
            AgentDomainEvent::SessionUpdated { .. }
        ));
        assert!(matches!(
            &events[1],
            AgentDomainEvent::TimelineUpsert { .. }
        ));
        let mut native = mirror.session_info(&asid).await.unwrap();
        native.runtime.as_mut().unwrap().source = RuntimeSource::Native;
        mirror.update_session(native).await;
        assert!(mirror
            .apply_observation(
                &asid,
                epoch,
                runtime,
                vec![text_item("late", "late", "late")],
                |_| {},
                |_| {}
            )
            .await
            .is_none());
        assert_eq!(mirror.get_snapshot(&asid).await.unwrap().timeline.len(), 1);
    }

    #[tokio::test]
    async fn explicit_refresh_fences_events_and_invalidates_old_replay_without_broadcast() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("fixture".into());
        mirror.update_session(info(&asid, "old")).await;
        mirror
            .upsert_timeline_items(&asid, vec![text_item("old", "old", "old")])
            .await;
        let captured = mirror.get_snapshot(&asid).await.unwrap();
        let mut fresh = captured.clone();
        fresh.timeline = vec![text_item("new", "new", "new")];
        mirror
            .update_status(&asid, AgentSessionStatus::Busy, None)
            .await;
        assert!(mirror
            .refresh_snapshot(fresh.clone(), Some(captured.seq))
            .await
            .is_none());
        let current = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(current.timeline[0].id, "old");
        fresh.info.status = AgentSessionStatus::Busy;
        let committed = mirror
            .refresh_snapshot(fresh, Some(current.seq))
            .await
            .unwrap();
        assert!(committed.seq > current.seq);
        assert!(mirror.get_events_after(&asid, current.seq).await.is_none());
        assert!(mirror
            .get_events_after(&asid, committed.seq)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            mirror.get_snapshot(&asid).await.unwrap().timeline[0].id,
            "new"
        );
        assert_eq!(mirror.event_log_len(&asid).await, 0);
    }

    #[test]
    fn replay_measures_serialized_bytes_exactly_and_enforces_count_and_bytes() {
        let event = replay_text(1, "你好🙂\n\"\\\t");
        assert_eq!(
            event_size(&event),
            serde_json::to_vec(&event).unwrap().len()
        );
        let mut state = SessionState::new(info(&AgentSessionId("s".into()), "test"));
        for seq in 1..=MAX_EVENT_LOG_ENTRIES as u64 + 3 {
            state.push_event(replay_text(seq, "x"));
        }
        assert_eq!(state.event_log.len(), MAX_EVENT_LOG_ENTRIES);
        assert_eq!(state.replay_floor, 3);
        assert_eq!(
            state.event_log_bytes,
            state.event_log.iter().map(|ev| ev.bytes).sum::<usize>()
        );
        for seq in MAX_EVENT_LOG_ENTRIES as u64 + 4..=MAX_EVENT_LOG_ENTRIES as u64 + 8 {
            state.push_event(replay_text(seq, &"x".repeat(MAX_EVENT_LOG_BYTES / 4)));
            assert!(state.event_log_bytes <= MAX_EVENT_LOG_BYTES);
        }
        assert_eq!(state.event_log.len(), 3, "serialized overhead counts too");
        assert_eq!(
            state.event_log_bytes,
            state
                .event_log
                .iter()
                .map(|ev| event_size(&ev.event))
                .sum::<usize>()
        );
        let floor = state.replay_floor;
        state.push_event(replay_text(
            MAX_EVENT_LOG_ENTRIES as u64 + 9,
            &"x".repeat(MAX_EVENT_LOG_BYTES),
        ));
        assert!(state.event_log.is_empty());
        assert_eq!(state.event_log_bytes, 0);
        assert!(state.replay_floor > floor);
    }

    #[test]
    fn replay_retains_exact_budget_but_not_a_lone_oversized_event() {
        let mut state = SessionState::new(info(&AgentSessionId("s".into()), "test"));
        let overhead = event_size(&replay_text(1, ""));
        let exact = replay_text(1, &"x".repeat(MAX_EVENT_LOG_BYTES - overhead));
        assert_eq!(event_size(&exact), MAX_EVENT_LOG_BYTES);
        state.push_event(exact);
        assert_eq!(state.event_log_bytes, MAX_EVENT_LOG_BYTES);
        state.push_event(replay_text(
            2,
            &"x".repeat(MAX_EVENT_LOG_BYTES - overhead + 1),
        ));
        assert!(state.event_log.is_empty());
        assert_eq!(state.replay_floor, 2);
        state.push_event(replay_text(3, "suffix"));
        assert_eq!(state.event_log.len(), 1);
        assert_eq!(state.event_log.front().unwrap().event.seq(), 3);
        assert_eq!(state.replay_floor, 2);
    }

    #[tokio::test]
    async fn count_eviction_refuses_old_cursors_but_accepts_the_replay_floor() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("count".into());
        for _ in 0..MAX_EVENT_LOG_ENTRIES + 3 {
            mirror
                .update_status(&asid, AgentSessionStatus::Idle, None)
                .await;
        }
        assert!(mirror.get_events_after(&asid, 2).await.is_none());
        assert!(mirror.get_timeline_delta(&asid, 2).await.2);
        let events = mirror.get_events_after(&asid, 3).await.unwrap();
        assert_eq!(events.len(), MAX_EVENT_LOG_ENTRIES);
        assert_eq!(events[0].seq(), 4);
        assert!(!mirror.get_timeline_delta(&asid, 3).await.2);
    }

    #[tokio::test]
    async fn empty_replay_floor_refuses_stale_cursors_and_snapshot_recovers_exact_text() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("s".into());
        mirror.update_session(info(&asid, "large transcript")).await;
        // No text/reasoning truncation: one authoritative row alone can be
        // larger than the replay budget, including the completion markers.
        let text = format!(
            "{}\nMUQUN_TASK_DONE: 完成🙂\nMUQUN_TASK_BLOCKED: 等待",
            "界🙂".repeat(MAX_EVENT_LOG_BYTES / 7 + 1)
        );
        let oversized_seq = mirror
            .upsert_timeline_items(&asid, vec![text_item("m:t0", "m", &text)])
            .await;
        assert_eq!(mirror.event_log_len(&asid).await, 0);
        for cursor in [0, oversized_seq - 1] {
            assert!(mirror.get_events_after(&asid, cursor).await.is_none());
            assert!(mirror.get_timeline_delta(&asid, cursor).await.2);
        }
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snapshot.seq, oversized_seq);
        assert_eq!(
            snapshot.timeline[0].part,
            AgentPart::Text { text: text.clone() }
        );
        assert!(mirror
            .get_events_after(&asid, snapshot.seq)
            .await
            .unwrap()
            .is_empty());
        assert!(!mirror.get_timeline_delta(&asid, snapshot.seq).await.2);
        // A small suffix after an oversize event is contiguous and replayable
        // from that snapshot; it cannot make an older cursor safe again.
        mirror
            .update_status(&asid, AgentSessionStatus::Idle, None)
            .await;
        let suffix = mirror.get_events_after(&asid, snapshot.seq).await.unwrap();
        assert_eq!(suffix.len(), 1);
        assert_eq!(suffix[0].seq(), snapshot.seq + 1);
        assert!(mirror
            .get_events_after(&asid, snapshot.seq - 1)
            .await
            .is_none());
        mirror
            .set_text_content(&asid, "m:r0", "m", 0, &text, true)
            .await
            .unwrap();
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        let reasoning = snapshot.timeline.iter().find(|it| it.id == "m:r0").unwrap();
        assert_eq!(
            reasoning.part,
            AgentPart::Reasoning {
                text,
                duration_ms: None
            }
        );
        assert_eq!(mirror.event_log_len(&asid).await, 0);
        assert!(mirror
            .get_events_after(&asid, snapshot.seq)
            .await
            .unwrap()
            .is_empty());
        assert!(!mirror.get_timeline_delta(&asid, snapshot.seq).await.2);
        assert!(mirror
            .get_events_after(&asid, snapshot.seq - 1)
            .await
            .is_none());
        assert!(mirror.get_timeline_delta(&asid, snapshot.seq - 1).await.2);
    }

    #[tokio::test]
    async fn inbox_delivery_update_requires_a_cached_item_and_records_the_full_queue() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-inbox".into());
        assert!(mirror
            .update_inbox_delivery(&asid, "missing", "steer")
            .await
            .is_none());
        assert!(mirror.get_snapshot(&asid).await.is_none());
        let mut items = vec![
            serde_json::json!({"id": "first", "delivery": "queue", "payload": {"text": "Keep"}}),
            serde_json::json!({"id": "second", "delivery": "queue", "payload": {"text": "Also keep"}}),
        ];
        let (before, _) = mirror.set_inbox(&asid, items.clone()).await;
        assert!(mirror
            .update_inbox_delivery(&asid, "missing", "steer")
            .await
            .is_none());
        assert_eq!(mirror.get_snapshot(&asid).await.unwrap().seq, before);
        let (seq, updated) = mirror
            .update_inbox_delivery(&asid, "first", "steer")
            .await
            .unwrap();
        items[0]["delivery"] = serde_json::json!("steer");
        assert_eq!(updated, items);
        assert_eq!(mirror.inbox(&asid).await, items);
        assert_eq!(seq, before + 1);
        let events = mirror.get_events_after(&asid, before).await.unwrap();
        assert_eq!(events.len(), 1);
        let AgentDomainEvent::InboxChanged {
            items: recorded, ..
        } = &events[0]
        else {
            panic!("delivery update did not record an inbox event");
        };
        assert_eq!(*recorded, items);
    }

    #[tokio::test]
    async fn read_markers_survive_refetch_and_duplicate_idle_events() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-read".into());
        mirror.update_session(info(&asid, "Read test")).await;
        mirror
            .update_status(&asid, AgentSessionStatus::Busy, None)
            .await;
        mirror
            .update_status(&asid, AgentSessionStatus::Idle, None)
            .await;
        let idle = mirror
            .get_snapshot(&asid)
            .await
            .unwrap()
            .info
            .time_idle
            .unwrap();
        mirror.mark_viewed(&asid, idle + 1).await.unwrap();
        mirror.mark_viewed(&asid, idle).await.unwrap();
        mirror
            .update_status(&asid, AgentSessionStatus::Idle, None)
            .await;
        mirror.update_session(info(&asid, "Read test")).await;
        let snap = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.info.time_idle, Some(idle));
        assert_eq!(snap.info.time_viewed, Some(idle + 1));
        let mut upstream = info(&asid, "Read test");
        mirror.overlay_read_markers(&mut upstream).await;
        assert_eq!(upstream.time_idle, Some(idle));
        assert_eq!(upstream.time_viewed, Some(idle + 1));
        assert!(mirror
            .mark_viewed(&AgentSessionId("missing".into()), idle)
            .await
            .is_none());
        // A read that says the session failed keeps the reason the failure
        // event gave.
        let error = AgentErrorInfo {
            name: "provider.auth".into(),
            message: "Key limit exceeded".into(),
            status: Some(403),
        };
        mirror
            .update_status(&asid, AgentSessionStatus::Failed, Some(error.clone()))
            .await;
        let mut failed = info(&asid, "Read test");
        failed.status = AgentSessionStatus::Failed;
        mirror.update_session(failed).await;
        let snap = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.info.error, Some(error));
    }

    #[tokio::test]
    async fn initial_history_is_distinct_from_metadata_and_preserves_newer_live_rows() {
        let mirror = MemoryMirror::for_agent("codex");
        let asid = AgentSessionId("initial-history".into());
        mirror.update_session(info(&asid, "Existing session")).await;
        let gate = mirror.history_gate(&asid).await;
        assert!(!mirror.history_initialized(&asid).await);
        let before = mirror.history_read_revision(&asid).await.unwrap();
        mirror
            .upsert_timeline_items(
                &asid,
                vec![text_item("answer", "turn", "Live completed answer")],
            )
            .await;
        assert!(
            mirror
                .initialize_history(
                    &asid,
                    &gate,
                    before,
                    vec![
                        text_item("question", "turn", "Original question"),
                        text_item("answer", "turn", "Stale partial answer"),
                    ]
                )
                .await
        );
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert!(mirror.history_initialized(&asid).await);
        assert_eq!(snapshot.info.agent_id, "codex");
        assert_eq!(snapshot.timeline.len(), 2);
        assert!(snapshot.timeline.iter().any(
            |item| matches!(&item.part, AgentPart::Text { text } if text == "Live completed answer")
        ));
        let second = vec![text_item(
            "unrequested",
            "turn",
            "Do not replace initialized history",
        )];
        assert!(
            mirror
                .initialize_history(&asid, &gate, (snapshot.seq, 0), second)
                .await
        );
        assert_eq!(
            mirror.get_snapshot(&asid).await.unwrap().timeline,
            snapshot.timeline
        );
    }

    #[tokio::test]
    async fn initial_history_cannot_resurrect_rows_removed_during_the_read() {
        for boundary in [false, true] {
            for cached in [false, true] {
                let mirror = MemoryMirror::for_agent("codex");
                let asid = AgentSessionId("removed-history".into());
                mirror.update_session(info(&asid, "Existing session")).await;
                let row = text_item("old-row", "msg_old", "Removed answer");
                if cached {
                    mirror.upsert_timeline_items(&asid, vec![row.clone()]).await;
                }
                let gate = mirror.history_gate(&asid).await;
                let before = mirror.history_read_revision(&asid).await.unwrap();
                if boundary {
                    mirror.remove_timeline_from(&asid, "msg_old").await;
                } else {
                    mirror
                        .remove_timeline_items(&asid, &["old-row".into()])
                        .await;
                }
                assert!(
                    !mirror
                        .initialize_history(&asid, &gate, before, vec![row])
                        .await
                );
                assert!(!mirror.history_initialized(&asid).await);
                assert!(mirror
                    .get_snapshot(&asid)
                    .await
                    .unwrap()
                    .timeline
                    .is_empty());
                let after = mirror.history_read_revision(&asid).await.unwrap();
                assert!(mirror.initialize_history(&asid, &gate, after, vec![]).await);
            }
        }
    }

    #[tokio::test]
    async fn initial_empty_history_is_cached_and_old_gate_cannot_initialize_recreated_session() {
        let mirror = MemoryMirror::for_agent("codex");
        let asid = AgentSessionId("empty-history".into());
        mirror.update_session(info(&asid, "Empty session")).await;
        let gate = mirror.history_gate(&asid).await;
        assert!(
            mirror
                .initialize_history(&asid, &gate, (1, 0), vec![])
                .await
        );
        assert!(mirror.history_initialized(&asid).await);
        mirror.sessions.write().await.remove(&asid);
        mirror
            .update_session(info(&asid, "Recreated session"))
            .await;
        assert!(
            !mirror
                .initialize_history(&asid, &gate, (1, 0), vec![])
                .await
        );
        assert!(!mirror.history_initialized(&asid).await);
    }

    #[tokio::test]
    async fn test_memory_mirror_lifecycle() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-mirror-test".to_string());

        let seq1 = mirror.update_session(info(&asid, "Test Session")).await;
        assert_eq!(seq1, 1);

        let snap = mirror
            .get_snapshot(&asid)
            .await
            .expect("snapshot should exist");
        assert_eq!(snap.info.title, "Test Session");
        assert_eq!(snap.timeline.len(), 0);

        let seq2 = mirror
            .upsert_timeline_items(&asid, vec![text_item("msg-1:t0", "msg-1", "Hello")])
            .await;
        assert_eq!(seq2, 2);

        let perm = PermissionRequest {
            id: "perm-1".to_string(),
            asid: asid.clone(),
            action: "execute".to_string(),
            resources: vec!["ls -la".to_string()],
            save: vec![],
            prompt: "List files".to_string(),
            tool: Some("bash".to_string()),
            source_message_id: None,
            source_tool_call_id: None,
            metadata: None,
            message: None,
            options: vec![],
        };
        let seq3 = mirror.add_permission(perm).await;
        assert_eq!(seq3, 3);

        let snap_perm = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap_perm.permissions.len(), 1);

        let seq4 = mirror.resolve_permission(&asid, "perm-1").await;
        assert_eq!(seq4, 4);

        let snap_resolved = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap_resolved.permissions.len(), 0);
    }

    #[tokio::test]
    async fn a_real_title_is_never_replaced_by_the_placeholder() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-title".to_string());
        mirror.update_session(info(&asid, "Real title")).await;

        // An event for a session the mirror already knows must not blank it.
        mirror
            .update_status(&asid, AgentSessionStatus::Busy, None)
            .await;
        mirror
            .merge_session_fields(
                &asid,
                SessionPatch {
                    agent: Some("plan".to_string()),
                    ..SessionPatch::default()
                },
            )
            .await;

        let snap = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.info.title, "Real title");
        assert_eq!(snap.info.mode.as_deref(), Some("plan"));
        assert_eq!(snap.info.status, AgentSessionStatus::Busy);
    }

    #[tokio::test]
    async fn an_unknown_session_gets_an_empty_placeholder_not_an_invented_one() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-unknown".to_string());
        mirror
            .update_status(&asid, AgentSessionStatus::Busy, None)
            .await;

        assert!(mirror.is_placeholder(&asid).await);
        let snap = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap.info.title, "");
        assert!(snap.info.mode.is_none());
    }

    #[tokio::test]
    async fn tool_events_are_joined_into_one_card() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-tool".to_string());
        mirror.update_session(info(&asid, "Tools")).await;

        mirror
            .upsert_tool_call(
                &asid,
                "msg-1",
                "call-1",
                ToolPatch {
                    name: Some("glob".to_string()),
                    state: Some(ToolCallStatus::Pending),
                    ..ToolPatch::default()
                },
            )
            .await
            .expect("created");

        mirror
            .upsert_tool_call(
                &asid,
                "msg-1",
                "call-1",
                ToolPatch {
                    input: Some(serde_json::json!({ "pattern": "**/*.rs" })),
                    state: Some(ToolCallStatus::Running),
                    ..ToolPatch::default()
                },
            )
            .await
            .expect("updated");

        let (_, item) = mirror
            .upsert_tool_call(
                &asid,
                "msg-1",
                "call-1",
                ToolPatch {
                    output: Some(serde_json::json!("a.rs\nb.rs")),
                    metadata: Some(serde_json::json!({ "count": 2, "truncated": false })),
                    state: Some(ToolCallStatus::Completed),
                    ..ToolPatch::default()
                },
            )
            .await
            .expect("completed");

        assert_eq!(mirror.timeline_len(&asid).await, 1);
        let AgentPart::Tool(call) = item.part else {
            panic!("expected a tool part");
        };
        assert_eq!(call.name, "glob");
        assert_eq!(call.title.as_deref(), Some("**/*.rs"));
        assert_eq!(call.state, ToolCallStatus::Completed);
        assert_eq!(call.status, ToolCallStatus::Completed);
        assert_eq!(
            call.output.as_ref().and_then(|v| v.as_str()),
            Some("a.rs\nb.rs")
        );
    }

    #[tokio::test]
    async fn late_tool_events_preserve_finished_state_in_events_and_snapshot() {
        for terminal in [ToolCallStatus::Completed, ToolCallStatus::Failed] {
            let mirror = MemoryMirror::new();
            let asid = AgentSessionId("ses-late-tool".to_string());
            mirror.update_session(info(&asid, "Late tool events")).await;
            mirror
                .upsert_tool_call(
                    &asid,
                    "msg-1",
                    "call-1",
                    ToolPatch {
                        name: Some("read".to_string()),
                        state: Some(terminal),
                        output: Some(serde_json::json!("result")),
                        completed_ms: Some(200),
                        ..ToolPatch::default()
                    },
                )
                .await
                .unwrap();
            for stale in [
                ToolCallStatus::Pending,
                ToolCallStatus::Streaming,
                ToolCallStatus::Running,
            ] {
                let (_, item) = mirror
                    .upsert_tool_call(
                        &asid,
                        "msg-1",
                        "call-1",
                        ToolPatch {
                            state: Some(stale),
                            metadata: Some(serde_json::json!({"count": 4})),
                            ..ToolPatch::default()
                        },
                    )
                    .await
                    .unwrap();
                let AgentPart::Tool(call) = item.part else {
                    panic!("expected tool")
                };
                assert_eq!(call.state, terminal);
                assert_eq!(call.status, terminal);
                assert_eq!(call.output, Some(serde_json::json!("result")));
                assert_eq!(call.time.completed, Some(200));
                assert_eq!(call.metadata, Some(serde_json::json!({"count": 4})));
            }
            let snapshot = mirror.get_snapshot(&asid).await.unwrap();
            let AgentPart::Tool(call) = &snapshot.timeline[0].part else {
                panic!("expected tool")
            };
            assert_eq!(call.state, terminal);
            assert_eq!(call.status, terminal);
        }
    }

    #[tokio::test]
    async fn a_giant_tool_output_is_truncated_and_flagged() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-big".to_string());
        mirror.update_session(info(&asid, "Big")).await;

        let huge = "x".repeat(MAX_TOOL_OUTPUT_BYTES * 3);
        let (_, item) = mirror
            .upsert_tool_call(
                &asid,
                "msg-1",
                "call-big",
                ToolPatch {
                    name: Some("read".to_string()),
                    output: Some(serde_json::json!(huge)),
                    state: Some(ToolCallStatus::Completed),
                    ..ToolPatch::default()
                },
            )
            .await
            .expect("created");

        let mut incoming = item.clone();
        if let AgentPart::Tool(call) = &mut incoming.part {
            call.output = Some(serde_json::json!(huge));
            call.truncated = false;
        }
        let AgentDomainEvent::TimelineUpsert { items, seq, .. } =
            mirror.record_timeline_upsert(&asid, vec![incoming]).await
        else {
            panic!("expected timeline event")
        };
        assert_eq!(items[0].seq, seq);
        let snapshot = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(
            serde_json::to_value(&items[0]).unwrap(),
            serde_json::to_value(&snapshot.timeline[0]).unwrap()
        );
        let AgentPart::Tool(call) = &items[0].part else {
            panic!("expected a tool part");
        };
        assert!(call.truncated, "an oversized result is flagged");
        let stored = call.output.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            stored.len() < MAX_TOOL_OUTPUT_BYTES + 128,
            "the stored output is capped, got {} bytes",
            stored.len()
        );
    }

    #[tokio::test]
    async fn the_timeline_and_event_log_stay_bounded() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-bounded".to_string());
        mirror.update_session(info(&asid, "Bounded")).await;

        for i in 0..(MAX_TIMELINE_ITEMS + 250) {
            let id = format!("msg-{i:06}:t0");
            mirror
                .upsert_timeline_items(&asid, vec![text_item(&id, &format!("msg-{i:06}"), "x")])
                .await;
        }

        assert!(mirror.timeline_len(&asid).await <= MAX_TIMELINE_ITEMS);
        assert!(mirror.event_log_len(&asid).await <= MAX_EVENT_LOG_ENTRIES);
    }

    #[tokio::test]
    async fn a_deleted_session_is_forgotten() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-gone".to_string());
        mirror.update_session(info(&asid, "Gone")).await;
        assert_eq!(mirror.session_count().await, 1);
        mirror.remove_session(&asid).await;
        assert_eq!(mirror.session_count().await, 0);
    }

    #[tokio::test]
    async fn the_session_table_is_capped() {
        let mirror = MemoryMirror::new();
        for i in 0..(MAX_SESSIONS + 40) {
            let asid = AgentSessionId(format!("ses-{i:05}"));
            mirror.update_session(info(&asid, "many")).await;
        }
        assert!(mirror.session_count().await <= MAX_SESSIONS);
    }
}
