use super::form::FormRequest;
use super::permission::PermissionRequest;
use super::session::{
    AgentErrorInfo, AgentSessionId, AgentSessionInfo, AgentSessionStatus, RevertState,
    SessionRevertInfo, WorktreeState,
};
use super::timeline::{CompactionStatus, TimelineItem};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentDomainEvent {
    #[serde(rename = "agent.session.updated")]
    SessionUpdated {
        asid: AgentSessionId,
        /// Boxed: this is by far the largest payload, and every event in the
        /// mirror's ring buffer would otherwise be sized for it.
        info: Box<AgentSessionInfo>,
        seq: u64,
    },
    #[serde(rename = "agent.timeline.upsert")]
    TimelineUpsert {
        asid: AgentSessionId,
        items: Vec<TimelineItem>,
        seq: u64,
    },
    #[serde(rename = "agent.timeline.removed")]
    TimelineRemoved {
        asid: AgentSessionId,
        ids: Vec<String>,
        seq: u64,
    },
    #[serde(rename = "agent.status.changed")]
    StatusChanged {
        asid: AgentSessionId,
        status: AgentSessionStatus,
        /// Set when the status is `failed`; `{name, message}`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<AgentErrorInfo>,
        seq: u64,
    },
    #[serde(rename = "agent.permission.pending")]
    PermissionPending {
        asid: AgentSessionId,
        request: PermissionRequest,
        seq: u64,
    },
    #[serde(rename = "agent.permission.resolved")]
    PermissionResolved {
        asid: AgentSessionId,
        request_id: String,
        seq: u64,
    },
    #[serde(rename = "agent.form.pending")]
    FormPending {
        asid: AgentSessionId,
        request: FormRequest,
        seq: u64,
    },
    #[serde(rename = "agent.form.resolved")]
    FormResolved {
        asid: AgentSessionId,
        form_id: String,
        seq: u64,
    },
    /// `session.compaction.*`, forwarded so the app can show a compaction
    /// running and replace the boundary row when it finishes.
    #[serde(rename = "agent.compaction.changed")]
    CompactionChanged {
        #[serde(rename = "session_id", alias = "asid")]
        asid: AgentSessionId,
        status: CompactionStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// The streamed summary text, on `session.compaction.delta`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delta: Option<String>,
        seq: u64,
    },
    /// `session.revert.staged|committed|cleared`: a rollback boundary the user
    /// can still cancel, that rollback applied, or the staging withdrawn.
    #[serde(rename = "agent.revert.changed")]
    RevertChanged {
        asid: AgentSessionId,
        state: RevertState,
        /// The staged boundary on `staged`. Always present as a field, and
        /// `null` on `committed` and `cleared`, where nothing is staged.
        revert: Option<SessionRevertInfo>,
        seq: u64,
    },
    /// `session.inbox.*`: the queued and steered items waiting for the agent
    /// loop, as a whole list so the app never has to reconcile a diff.
    #[serde(rename = "agent.inbox.changed")]
    InboxChanged {
        #[serde(rename = "session_id", alias = "asid")]
        asid: AgentSessionId,
        items: Vec<serde_json::Value>,
        seq: u64,
    },
    /// A project's worktrees changed. Not a session's event: like
    /// `agent.resync` it carries an empty `asid`, which is how it reaches
    /// every agent-session stream as well as the device-wide session stream.
    #[serde(rename = "agent.worktree.changed")]
    WorktreeChanged {
        #[serde(skip_serializing)]
        asid: AgentSessionId,
        state: WorktreeState,
        /// The project directory the change belongs to, from the event
        /// envelope's own `location.directory`, or the resolved worktree on
        /// `worktree.resolved`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directory: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<String>,
        /// The worktree's name, on `ready`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// The ref it was branched from, on `ready`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// The message, on `failed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "agent.resync")]
    Resync {
        asid: AgentSessionId,
        reason: String,
    },
}

impl AgentDomainEvent {
    pub fn seq(&self) -> u64 {
        match self {
            Self::SessionUpdated { seq, .. } => *seq,
            Self::TimelineUpsert { seq, .. } => *seq,
            Self::TimelineRemoved { seq, .. } => *seq,
            Self::StatusChanged { seq, .. } => *seq,
            Self::PermissionPending { seq, .. } => *seq,
            Self::PermissionResolved { seq, .. } => *seq,
            Self::FormPending { seq, .. } => *seq,
            Self::FormResolved { seq, .. } => *seq,
            Self::CompactionChanged { seq, .. } => *seq,
            Self::RevertChanged { seq, .. } => *seq,
            Self::InboxChanged { seq, .. } => *seq,
            Self::WorktreeChanged { .. } => 0,
            Self::Resync { .. } => 0,
        }
    }

    /// The SSE event name this domain event is published under. One table, so
    /// a new variant cannot be added to the enum and forgotten at a stream.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::SessionUpdated { .. } => "agent.session.updated",
            Self::TimelineUpsert { .. } => "agent.timeline.upsert",
            Self::TimelineRemoved { .. } => "agent.timeline.removed",
            Self::StatusChanged { .. } => "agent.status.changed",
            Self::PermissionPending { .. } => "agent.permission.pending",
            Self::PermissionResolved { .. } => "agent.permission.resolved",
            Self::FormPending { .. } => "agent.form.pending",
            Self::FormResolved { .. } => "agent.form.resolved",
            Self::CompactionChanged { .. } => "agent.compaction.changed",
            Self::RevertChanged { .. } => "agent.revert.changed",
            Self::InboxChanged { .. } => "agent.inbox.changed",
            Self::WorktreeChanged { .. } => "agent.worktree.changed",
            Self::Resync { .. } => "agent.resync",
        }
    }

    pub fn asid(&self) -> &AgentSessionId {
        match self {
            Self::SessionUpdated { asid, .. } => asid,
            Self::TimelineUpsert { asid, .. } => asid,
            Self::TimelineRemoved { asid, .. } => asid,
            Self::StatusChanged { asid, .. } => asid,
            Self::PermissionPending { asid, .. } => asid,
            Self::PermissionResolved { asid, .. } => asid,
            Self::FormPending { asid, .. } => asid,
            Self::FormResolved { asid, .. } => asid,
            Self::CompactionChanged { asid, .. } => asid,
            Self::RevertChanged { asid, .. } => asid,
            Self::InboxChanged { asid, .. } => asid,
            Self::WorktreeChanged { asid, .. } => asid,
            Self::Resync { asid, .. } => asid,
        }
    }
}
