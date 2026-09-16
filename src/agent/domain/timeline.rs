use serde::{Deserialize, Serialize};
use super::permission::PermissionRequest;
use super::form::FormRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentPart {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Tool {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
        status: ToolCallStatus,
    },
    Diff {
        file: String,
        diff: String,
    },
    Todo {
        items: Vec<TodoItem>,
    },
    Approval {
        request: PermissionRequest,
    },
    Form {
        request: FormRequest,
    },
    Status {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineItem {
    pub id: String,
    pub message_id: String,
    pub role: TimelineRole,
    pub part: AgentPart,
    pub seq: u64,
    pub updated_ms: u64,
}
