use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentSessionId(pub String);

impl std::fmt::Display for AgentSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for AgentSessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for AgentSessionId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    Idle,
    Busy,
    Retry,
    Failed,
    /// The run was aborted by the user (`session.execution.interrupted`).
    Interrupted,
    Unknown,
}

/// An error surfaced alongside a status change, from
/// `session.execution.failed`'s `Session.StructuredError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentErrorInfo {
    pub name: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TokensUsage {
    pub input: u64,
    pub output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
}

/// The filters `GET /api/session` accepts, in the gateway's own vocabulary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub directory: Option<String>,
    /// A session id to list the children of, or `Some("null")` for top-level
    /// sessions only. OpenCode takes the literal string `null` for that.
    pub parent_id: Option<String>,
    pub limit: Option<usize>,
    /// `asc` or `desc`.
    pub order: Option<String>,
    pub search: Option<String>,
    pub cursor: Option<String>,
}

/// `Session.Info.revert`: a staged rollback the user can still cancel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRevertInfo {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// `FileDiff.Info[]` verbatim, when the stage carried file changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<serde_json::Value>,
}

/// `Session.Info.fork`: where a forked session was copied from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForkInfo {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionInfo {
    pub asid: AgentSessionId,
    pub backend_session_id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    pub status: AgentSessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokensUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// `Session.Info.outcome`: `succeeded`, `failed` or `interrupted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The error behind a `failed` status, when one was reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentErrorInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revert: Option<SessionRevertInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<SessionForkInfo>,
    /// `time.idle`: when the session last went idle. With `time_viewed`, this
    /// is the unread rule -- unread when `time_idle > time_viewed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_idle: Option<u64>,
    /// `time.viewed`: when the user last acknowledged the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_viewed: Option<u64>,
    /// Set once OpenCode reports the session gone, so a client holding it open
    /// is told rather than left polling a 404.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deleted: bool,
    pub updated_ms: u64,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProject {
    pub id: String,
    pub canonical: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs: Option<String>,
    #[serde(default)]
    pub sandboxes: Vec<String>,
}

