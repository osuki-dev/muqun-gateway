use serde::{Deserialize, Serialize};
use super::form::FormRequest;
use super::permission::PermissionRequest;
use super::session::{AgentErrorInfo, ModelRef, TokensUsage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineRole {
    User,
    Assistant,
    System,
}

/// The state of one tool call. OpenCode's own set is
/// `streaming | running | completed | error`; `pending` covers the window
/// between `session.tool.input.started` (which carries the name) and
/// `session.tool.called` (which carries the input).
///
/// `failed` is the wire name for the error state, kept from the previous
/// release so existing clients keep working; `error` is accepted on the way in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Streaming,
    Running,
    Completed,
    #[serde(rename = "failed", alias = "error")]
    Failed,
}

/// `Session.Message.Assistant.Tool.time`. `ran` is dispatch and `completed` is
/// finish, so the duration is `completed - ran`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolTime {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
}

impl ToolTime {
    pub fn is_empty(&self) -> bool {
        self.created.is_none() && self.ran.is_none() && self.completed.is_none()
    }
}

/// One tool call, assembled either from a message refetch or incrementally from
/// `session.tool.*` events joined on the call id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A short label for the card header. OpenCode has no `title` field, so
    /// this is derived from the tool name and its input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub input: serde_json::Value,
    /// Text content joined into one string, kept for clients written against
    /// the previous release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    /// `Tool.Content[]` verbatim: `{type:"text", text}` and
    /// `{type:"file", uri, mime, name?}` items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    /// The tool's own metadata, verbatim and therefore in OpenCode's camelCase:
    /// `files` (a `FileDiff.Info[]`) for `edit`, `sessionID`/`status` for
    /// `subagent`, `exit`/`status` for `shell`, `truncated` for all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// The tool state. `status` is the same value under the name the previous
    /// release used.
    pub state: ToolCallStatus,
    pub status: ToolCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AgentErrorInfo>,
    /// The child session a `subagent`/`task` call is driving, lifted out of
    /// `metadata.sessionID` so the app can deep-link while it is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    /// The tool was detached by `POST .../background` or reports itself as
    /// backgrounded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
    /// `metadata.truncated`: the result the user is reading is clipped.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "ToolTime::is_empty")]
    pub time: ToolTime,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl ToolCall {
    pub fn set_state(&mut self, state: ToolCallStatus) {
        self.state = state;
        self.status = state;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStatus {
    Started,
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
    Tool(ToolCall),
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
    /// A compaction boundary: `Session.Message.Compaction`. `summary` is the
    /// compacted history, `recent` the retained tail.
    Compaction {
        status: CompactionStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        recent: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<TokensUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<AgentErrorInfo>,
    },
    /// `Session.Message.Skill`: a skill the agent activated.
    Skill {
        skill: String,
        name: String,
        text: String,
    },
    /// `Session.Message.Shell`: a shell run outside the tool loop, including
    /// one detached into the background.
    Shell {
        shell_id: String,
        command: String,
        status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },
    /// `Session.Message.ModelSelected`.
    ModelSwitched {
        model: ModelRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<ModelRef>,
    },
    /// `Session.Message.AgentSelected`.
    AgentSwitched {
        agent: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
    /// `Session.Message.Synthetic`: text injected into the history that the
    /// user did not type.
    Synthetic {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// `Session.Message.System`.
    System {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// `Session.Message.LocationSwitched`: the session moved directory.
    LocationSwitched {
        directory: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        previous: Option<String>,
    },
}

/// How a part sorts against its siblings when two share an ordinal, which the
/// live stream does: reasoning and text both start at ordinal 0.
fn kind_rank(part: &AgentPart) -> u8 {
    match part {
        AgentPart::Reasoning { .. } => 0,
        AgentPart::Text { .. } => 1,
        AgentPart::Tool(_) => 2,
        _ => 3,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineItem {
    pub id: String,
    pub message_id: String,
    pub role: TimelineRole,
    pub part: AgentPart,
    pub seq: u64,
    pub updated_ms: u64,
    /// Position within the message. Exact once the message has been read back
    /// from OpenCode; while streaming it is the event's own ordinal.
    #[serde(default)]
    pub ordinal: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<String>>,
}

impl TimelineItem {
    /// Message ids are sortable by creation, so `(message_id, ordinal, kind)`
    /// is chronological order without a second timestamp lookup.
    pub fn sort_key(&self) -> (&str, u64, u8, &str) {
        (
            self.message_id.as_str(),
            self.ordinal,
            kind_rank(&self.part),
            self.id.as_str(),
        )
    }
}

/// Timeline item ids, shared by the streaming path and the refetch path so the
/// two never produce two rows for one part.
pub fn text_item_id(message_id: &str, ordinal: u64) -> String {
    format!("{message_id}:t{ordinal}")
}

pub fn reasoning_item_id(message_id: &str, ordinal: u64) -> String {
    format!("{message_id}:r{ordinal}")
}

pub fn tool_item_id(message_id: &str, tool_call_id: &str) -> String {
    format!("{message_id}:tool:{tool_call_id}")
}

pub fn part_item_id(message_id: &str, ordinal: u64) -> String {
    format!("{message_id}:p{ordinal}")
}
