use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::domain::{
    tool_item_id, AgentDomainEvent, AgentErrorInfo, AgentPart, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, FormRequest, PermissionRequest, RevertState, SessionRevertInfo,
    TimelineItem, TimelineRole, ToolCall, ToolCallStatus, ToolTime,
};
use crate::agent::ports::mirror::{AgentSessionSnapshot, MirrorFuture, SessionMirrorPort};

/// Ring-buffer bounds. The mirror is a cache in front of OpenCode, not a store:
/// everything it drops can be refetched, and a client that falls behind is told
/// to resync. Nothing here may grow with the length of a session.
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
        backend_session_id: asid.0.clone(),
        title: String::new(),
        agent: None,
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
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(0)
}

struct SessionState {
    info: AgentSessionInfo,
    timeline: BTreeMap<String, TimelineItem>,
    permissions: HashMap<String, PermissionRequest>,
    forms: HashMap<String, FormRequest>,
    inbox: Vec<serde_json::Value>,
    event_log: VecDeque<AgentDomainEvent>,
    event_log_bytes: usize,
    current_seq: u64,
    touched_ms: u64,
    /// True while `info` is the local stand-in rather than something OpenCode
    /// reported.
    placeholder: bool,
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
            current_seq: 0,
            touched_ms: now_ms(),
            placeholder: false,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.current_seq += 1;
        self.touched_ms = now_ms();
        self.current_seq
    }

    fn push_event(&mut self, event: AgentDomainEvent) {
        let size = event_size(&event);
        self.event_log_bytes += size;
        self.event_log.push_back(event);
        while self.event_log.len() > MAX_EVENT_LOG_ENTRIES
            || (self.event_log_bytes > MAX_EVENT_LOG_BYTES && self.event_log.len() > 1)
        {
            if let Some(dropped) = self.event_log.pop_front() {
                self.event_log_bytes = self.event_log_bytes.saturating_sub(event_size(&dropped));
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
}

impl MemoryMirror {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
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
        sessions: &'a mut HashMap<AgentSessionId, SessionState>,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
    ) -> &'a mut SessionState {
        if !sessions.contains_key(asid) {
            let mut fresh = SessionState::new(placeholder_session(asid, status));
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

    pub async fn update_status(
        &self,
        asid: &AgentSessionId,
        status: AgentSessionStatus,
        error: Option<AgentErrorInfo>,
    ) -> Option<u64> {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
        state.info.status = status;
        if error.is_some() {
            state.info.error = error.clone();
        } else if status != AgentSessionStatus::Failed {
            state.info.error = None;
        }
        if status == AgentSessionStatus::Idle {
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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);

        if let Some(title) = patch.title {
            if !title.trim().is_empty() {
                state.info.title = title;
                state.placeholder = false;
            }
        }
        if let Some(agent) = patch.agent {
            state.info.agent = Some(agent);
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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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

    /// Record an inbox item the stream announced, without a round trip.
    pub async fn upsert_inbox_item(
        &self,
        asid: &AgentSessionId,
        inbox_id: &str,
        item: Option<serde_json::Value>,
    ) -> (u64, Vec<serde_json::Value>) {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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

    pub async fn record_compaction(
        &self,
        asid: &AgentSessionId,
        status: crate::agent::domain::CompactionStatus,
        reason: Option<String>,
        delta: Option<String>,
    ) -> u64 {
        let mut sessions = self.sessions.write().await;
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Busy);
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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Busy);

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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Busy);

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

        // A client that has never synced (`after=0`) is told to resync too
        // whenever the log no longer starts at the beginning.
        if let Some(first) = state.event_log.front() {
            if after_seq < first.seq().saturating_sub(1) {
                return (Vec::new(), status, true, current_seq);
            }
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
        tokens: Option<crate::agent::domain::TokensUsage>,
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
        let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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
    pub model: Option<crate::agent::domain::ModelRef>,
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
            crate::agent::domain::push_input_partial(
                call.input_partial.get_or_insert_with(String::new),
                delta,
            );
        }
        if let Some(ref input) = self.input {
            call.input = input.clone();
            call.title = crate::agent::adapters::opencode::mapper::tool_title(&call.name, input);
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
        crate::agent::adapters::opencode::mapper::apply_tool_metadata(call);
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

            if let Some(first) = state.event_log.front() {
                // The log starts at `first.seq()`, so it can answer for any
                // client at or past `first.seq() - 1`. Anything earlier --
                // including a client that has never synced -- must resync.
                if after_seq < first.seq().saturating_sub(1) {
                    return None;
                }
            }

            let events: Vec<AgentDomainEvent> = state
                .event_log
                .iter()
                .filter(|ev| ev.seq() > after_seq)
                .cloned()
                .collect();
            Some(events)
        })
    }

    fn update_session<'a>(&'a self, info: AgentSessionInfo) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
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
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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
            state.push_event(event);
            seq
        })
    }

    fn add_permission<'a>(&'a self, request: PermissionRequest) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = request.asid.clone();
            // A permission raised on a session the mirror has not otherwise
            // seen used to be dropped on the floor and answered with `seq: 0`.
            let state = Self::entry(&mut sessions, &asid, AgentSessionStatus::Busy);
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
            let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
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
            let state = Self::entry(&mut sessions, &asid, AgentSessionStatus::Busy);
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
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let state = Self::entry(&mut sessions, asid, AgentSessionStatus::Idle);
            state.forms.remove(form_id);
            let seq = state.next_seq();

            let event = AgentDomainEvent::FormResolved {
                asid: asid.clone(),
                form_id: form_id.to_string(),
                seq,
            };
            state.push_event(event);
            seq
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::domain::{AgentPart, AgentSessionInfo, AgentSessionStatus, TimelineRole};

    fn info(asid: &AgentSessionId, title: &str) -> AgentSessionInfo {
        let mut info = placeholder_session(asid, AgentSessionStatus::Idle);
        info.title = title.to_string();
        info.agent = Some("build".to_string());
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
        }
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
        assert_eq!(snap.info.agent.as_deref(), Some("plan"));
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
        assert!(snap.info.agent.is_none());
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

        let AgentPart::Tool(call) = item.part else {
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
