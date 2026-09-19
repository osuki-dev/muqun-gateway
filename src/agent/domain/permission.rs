use serde::{Deserialize, Serialize};
use super::session::AgentSessionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowAlways,
    Deny,
}

impl PermissionDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowAlways => "allow_always",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub index: usize,
    pub label: String,
    pub decision: PermissionDecision,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: String,
    pub asid: AgentSessionId,
    pub action: String,
    pub resources: Vec<String>,
    /// The patterns an "always" reply would persist project-wide
    /// (`Permission.Request.save`). Empty means OpenCode offered no such reply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub save: Vec<String>,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// `Permission.Request.source.messageID`: the message the prompting tool
    /// call belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
    /// `Permission.Request.source.id`: the tool call id, so the app can attach
    /// the prompt to the exact tool card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub options: Vec<PermissionOption>,
}
