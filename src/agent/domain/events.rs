use serde::{Deserialize, Serialize};
use super::session::{AgentErrorInfo, AgentSessionId, AgentSessionInfo, AgentSessionStatus};
use super::timeline::TimelineItem;
use super::permission::PermissionRequest;
use super::form::FormRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentDomainEvent {
    #[serde(rename = "agent.session.updated")]
    SessionUpdated {
        asid: AgentSessionId,
        info: AgentSessionInfo,
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
            Self::Resync { .. } => 0,
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
            Self::Resync { asid, .. } => asid,
        }
    }
}
