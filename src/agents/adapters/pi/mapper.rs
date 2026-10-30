//! Pi wire records -> Muqun domain values.
//!
//! Two inputs share one mapping: persisted session entries (the active branch
//! of a session file, read without a child) and the live RPC event stream of a
//! running child. Both go through [`message_items`], so a message streamed live
//! and the same message read back from the file produce the same rows.
//!
//! Everything arrives as raw `serde_json::Value`: Pi adds entry types, content
//! blocks and roles between releases, and an unknown one is skipped rather than
//! failing the whole timeline.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::agents::adapters::t3::client::ms_from_iso;
use crate::agents::domain::{
    AgentCatalog, AgentPart, AgentSessionId, AgentSessionStatus, CatalogDefaults, CompactionStatus,
    ModelInfo, ModelRef, ModelVariantInfo, PermissionDecision, PermissionOption, PermissionRequest,
    TimelineItem, TimelineRole, TokensUsage, ToolCall, ToolCallStatus, ToolTime,
};

/// The bundled Pi extension: approval gate plus the Muqun MCP server. Written
/// to disk and passed to `pi -e` so the gateway never depends on the user's
/// own extension directory.
pub const EXTENSION_SOURCE: &str = include_str!("muqun-extension.js");

/// The `confirm` title the extension uses for a tool approval. Any other
/// dialog is not ours to answer and is cancelled.
pub const PERMISSION_TITLE: &str = "muqun.permission";

/// Prefix of the ids minted for a live message before Pi has persisted it.
const PROVISIONAL_PREFIX: &str = "live-";

/// How many live messages the folder remembers for re-keying and late tool
/// events. A run streams one assistant message per turn; this covers far more
/// turns than sit between a message and its tool results.
const MAX_LIVE_MESSAGES: usize = 64;

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The timeline orders rows by `message_id`, and Pi entry ids are random hex,
/// so the id is prefixed with the message's creation time. The message
/// timestamp is the same number live (`message_start`) and on disk, which keeps
/// a re-keyed live row and the reloaded row identical.
fn message_id(sort_ms: u64, key: &str) -> String {
    format!("{sort_ms:020}:{key}")
}

/// `content` as a string, or the text blocks of a content array joined.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| text(b, "type") == Some("text"))
            .filter_map(|b| text(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn item(
    id: String,
    message_id: String,
    role: TimelineRole,
    part: AgentPart,
    ordinal: u64,
    updated_ms: u64,
) -> TimelineItem {
    TimelineItem {
        id,
        message_id,
        role,
        part,
        seq: 0,
        updated_ms,
        ordinal,
        attachments: None,
        image_assets: None,
        generation: None,
    }
}

fn tool_call(id: &str, name: &str, input: Value, state: ToolCallStatus) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        title: None,
        input,
        output: None,
        content: None,
        metadata: None,
        state,
        status: state,
        error: None,
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated: false,
        time: ToolTime::default(),
    }
}

/// What execution has told us about a tool call, kept apart from the message
/// so the authoritative `message_end` mapping does not reset a call that
/// already ran (Pi may report the run before or after the message closes).
#[derive(Debug, Clone)]
struct ToolOutcome {
    state: ToolCallStatus,
    input: Option<Value>,
    /// `{content, details}`: a `tool_execution_end.result` or a `toolResult`
    /// message, which share that shape.
    result: Option<Value>,
}

impl ToolOutcome {
    fn finished(is_error: bool, result: &Value) -> Self {
        Self {
            state: if is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            },
            input: None,
            result: Some(result.clone()),
        }
    }

    fn apply(&self, call: &mut ToolCall) {
        call.set_state(self.state);
        if let Some(input) = &self.input {
            call.input = input.clone();
        }
        if let Some(result) = &self.result {
            let content = result.get("content");
            call.output = Some(Value::String(content_text(content)));
            call.content = content.cloned();
            call.metadata = result.get("details").filter(|d| !d.is_null()).cloned();
        }
    }
}

/// The rows of one `user` or `assistant` message, keyed under `key` (the entry
/// id, or a provisional id while live). Other roles produce nothing: tool
/// results complete a tool row instead, and system/custom messages are not
/// conversation.
///
/// Row ids are `key` for a single-block message and `key:<contentIndex>`
/// otherwise; `ordinal` is the content index. `seq` is left for the caller.
fn message_items(
    key: &str,
    updated_ms: u64,
    message: &Value,
    outcomes: &HashMap<String, ToolOutcome>,
) -> Vec<TimelineItem> {
    let sort_ms = message
        .get("timestamp")
        .and_then(Value::as_u64)
        .unwrap_or(updated_ms);
    let mid = message_id(sort_ms, key);
    match text(message, "role") {
        Some("user") => {
            let body = content_text(message.get("content"));
            if body.is_empty() {
                return vec![];
            }
            vec![item(
                key.to_string(),
                mid,
                TimelineRole::User,
                AgentPart::Text { text: body },
                0,
                updated_ms,
            )]
        }
        Some("assistant") => {
            let blocks = message
                .get("content")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let multi = blocks.len() > 1;
            blocks
                .iter()
                .enumerate()
                .filter_map(|(index, block)| {
                    let part = block_part(block, outcomes)?;
                    let id = if multi {
                        format!("{key}:{index}")
                    } else {
                        key.to_string()
                    };
                    Some(item(
                        id,
                        mid.clone(),
                        TimelineRole::Assistant,
                        part,
                        index as u64,
                        updated_ms,
                    ))
                })
                .collect()
        }
        _ => vec![],
    }
}

fn block_part(block: &Value, outcomes: &HashMap<String, ToolOutcome>) -> Option<AgentPart> {
    match text(block, "type")? {
        "text" => {
            let body = text(block, "text").filter(|t| !t.is_empty())?;
            Some(AgentPart::Text {
                text: body.to_string(),
            })
        }
        // A redacted block has no visible text; an empty card says nothing.
        "thinking" => {
            let body = text(block, "thinking").filter(|t| !t.is_empty())?;
            Some(AgentPart::Reasoning {
                text: body.to_string(),
                duration_ms: None,
            })
        }
        "toolCall" => {
            let id = text(block, "id")?;
            let input = block.get("arguments").cloned().unwrap_or(Value::Null);
            let mut call = tool_call(
                id,
                text(block, "name").unwrap_or("tool"),
                input,
                ToolCallStatus::Running,
            );
            if let Some(outcome) = outcomes.get(id) {
                outcome.apply(&mut call);
            }
            Some(AgentPart::Tool(call))
        }
        _ => None,
    }
}

fn model_ref(entry: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider_id: text(entry, "provider")?.to_string(),
        model_id: text(entry, "modelId")?.to_string(),
        variant: None,
    })
}

/// Map the active branch of a session file (root first, header excluded) to
/// timeline rows. `seq` is the row's position.
pub fn timeline_from_entries(entries: &[Value]) -> Vec<TimelineItem> {
    let no_outcomes = HashMap::new();
    let mut items: Vec<TimelineItem> = Vec::new();
    let mut tool_rows: HashMap<String, usize> = HashMap::new();
    let mut previous_model: Option<ModelRef> = None;
    for entry in entries {
        let Some(id) = text(entry, "id") else {
            continue;
        };
        let ms = text(entry, "timestamp").and_then(ms_from_iso).unwrap_or(0);
        let new_items = match text(entry, "type") {
            Some("message") => {
                let Some(message) = entry.get("message") else {
                    continue;
                };
                if text(message, "role") == Some("toolResult") {
                    let row = text(message, "toolCallId").and_then(|c| tool_rows.get(c));
                    if let Some(row) = row.and_then(|&at| items.get_mut(at)) {
                        if let AgentPart::Tool(call) = &mut row.part {
                            let is_error = message.get("isError").and_then(Value::as_bool);
                            ToolOutcome::finished(is_error == Some(true), message).apply(call);
                            row.updated_ms = ms;
                        }
                    }
                    continue;
                }
                message_items(id, ms, message, &no_outcomes)
            }
            Some("compaction") => vec![item(
                id.to_string(),
                message_id(ms, id),
                TimelineRole::System,
                AgentPart::Compaction {
                    status: CompactionStatus::Completed,
                    reason: None,
                    summary: text(entry, "summary").map(str::to_string),
                    recent: None,
                    keep_message_id: None,
                    tokens: None,
                    cost: None,
                    error: None,
                },
                0,
                ms,
            )],
            Some("model_change") => {
                let Some(model) = model_ref(entry) else {
                    continue;
                };
                let previous = previous_model.replace(model.clone());
                vec![item(
                    id.to_string(),
                    message_id(ms, id),
                    TimelineRole::System,
                    AgentPart::ModelSwitched { model, previous },
                    0,
                    ms,
                )]
            }
            _ => continue,
        };
        for mut row in new_items {
            row.seq = items.len() as u64;
            if let AgentPart::Tool(call) = &row.part {
                tool_rows.insert(call.id.clone(), items.len());
            }
            items.push(row);
        }
    }
    items
}

/// The permission card line and the resources it names, for the three tools
/// the extension gates.
pub fn permission_prompt(tool: &str, input: &Value) -> (String, Vec<String>) {
    let field = match tool {
        "bash" => "command",
        "edit" | "write" => "path",
        _ => return (tool.to_string(), vec![]),
    };
    match text(input, field) {
        Some(value) if tool == "bash" => (format!("bash: {value}"), vec![value.to_string()]),
        Some(value) => (format!("{tool} {value}"), vec![value.to_string()]),
        None => (tool.to_string(), vec![]),
    }
}

/// `get_available_models.data`, `get_available_thinking_levels.data` and
/// `get_state.data.model` -> catalog.
///
/// Pi reports thinking levels for the current model only; they are offered as
/// variants on every reasoning model because Pi clamps an unsupported level to
/// the nearest one the model has rather than rejecting it.
pub fn catalog(models: &Value, levels: &Value, current: Option<&Value>) -> AgentCatalog {
    let variants: Vec<ModelVariantInfo> = levels
        .get("levels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|level| ModelVariantInfo {
            id: level.to_string(),
            reasoning_effort: Some(level.to_string()),
        })
        .collect();
    let models = models
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let id = text(model, "id")?;
            let reasoning = model.get("reasoning").and_then(Value::as_bool) == Some(true);
            let limit = model.get("contextWindow").map(|context| {
                serde_json::json!({ "context": context, "output": model.get("maxTokens") })
            });
            Some(ModelInfo {
                id: id.to_string(),
                name: text(model, "name").unwrap_or(id).to_string(),
                provider_id: text(model, "provider")?.to_string(),
                family: None,
                limit,
                variants: (reasoning && !variants.is_empty()).then(|| variants.clone()),
                cost: model.get("cost").cloned(),
                enabled: true,
                status: None,
            })
        })
        .collect();
    let model = current.and_then(|m| {
        Some(ModelRef {
            provider_id: text(m, "provider")?.to_string(),
            model_id: text(m, "id")?.to_string(),
            variant: None,
        })
    });
    AgentCatalog {
        models,
        defaults: CatalogDefaults { model, mode: None },
        ..AgentCatalog::default()
    }
}

/// One change a live record makes to a session.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveChange {
    Status(AgentSessionStatus),
    Upsert(Vec<TimelineItem>),
    Remove(Vec<String>),
    Permission(PermissionRequest),
    PermissionResolved(String),
    Title(String),
    Compaction {
        status: CompactionStatus,
        summary: Option<String>,
    },
    Usage {
        tokens: TokensUsage,
        cost: Option<f64>,
        total_tokens: u64,
    },
    /// An `extension_ui_request` dialog that is not ours: answer it
    /// `{cancelled: true}` so Pi does not wait on a prompt nobody sees.
    UiCancel(String),
}

/// A message streamed by the live child. `key` is a provisional `live-<n>`
/// until the persisted entry is seen, then the entry id.
#[derive(Debug)]
struct LiveMessage {
    key: String,
    role: String,
    timestamp: u64,
    items: Vec<TimelineItem>,
}

/// Folds one session's RPC records into [`LiveChange`]s.
///
/// Pi sends no entry id with a live message, so rows are keyed provisionally
/// and re-keyed when an `entry_appended` carrying the persisted message
/// arrives (matched on role and message timestamp).
#[derive(Debug)]
pub struct LiveFolder {
    asid: AgentSessionId,
    seq: u64,
    minted: u64,
    messages: Vec<LiveMessage>,
    current: Option<String>,
    outcomes: HashMap<String, ToolOutcome>,
    pending_permissions: HashSet<String>,
    last_total_tokens: u64,
}

impl LiveFolder {
    pub fn new(asid: AgentSessionId) -> Self {
        Self {
            asid,
            seq: 0,
            minted: 0,
            messages: Vec::new(),
            current: None,
            outcomes: HashMap::new(),
            pending_permissions: HashSet::new(),
            last_total_tokens: 0,
        }
    }

    /// Start live `seq` numbers after the `n` rows already read from the
    /// session file, so live rows sort after history.
    pub fn with_history_len(mut self, n: usize) -> Self {
        self.seq = n as u64;
        self
    }

    pub fn fold(&mut self, record: &Value) -> Vec<LiveChange> {
        match text(record, "type").unwrap_or_default() {
            "agent_start" => vec![LiveChange::Status(AgentSessionStatus::Busy)],
            "agent_settled" => {
                let aborted = record.get("aborted").and_then(Value::as_bool) == Some(true);
                vec![LiveChange::Status(if aborted {
                    AgentSessionStatus::Interrupted
                } else {
                    AgentSessionStatus::Idle
                })]
            }
            "auto_retry_start" => vec![LiveChange::Status(AgentSessionStatus::Retry)],
            "message_start" => {
                if let Some(message) = record.get("message") {
                    if text(message, "role") == Some("assistant") {
                        let key = self.open(message);
                        self.current = Some(key);
                    }
                }
                vec![]
            }
            "message_update" => self.on_update(record),
            "message_end" => record
                .get("message")
                .map(|m| self.on_message_end(m))
                .unwrap_or_default(),
            "tool_execution_start" | "tool_execution_end" => self.on_tool_execution(record),
            "entry_appended" => record
                .get("entry")
                .map(|e| self.on_entry(e))
                .unwrap_or_default(),
            "compaction_start" => vec![LiveChange::Compaction {
                status: CompactionStatus::Started,
                summary: None,
            }],
            "compaction_end" => {
                let summary = record
                    .pointer("/result/summary")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let aborted = record.get("aborted").and_then(Value::as_bool) == Some(true);
                let status = if summary.is_some() && !aborted {
                    CompactionStatus::Completed
                } else {
                    CompactionStatus::Failed
                };
                vec![LiveChange::Compaction { status, summary }]
            }
            "session_info_changed" => text(record, "name")
                .map(|name| vec![LiveChange::Title(name.to_string())])
                .unwrap_or_default(),
            "extension_ui_request" => self.on_ui_request(record),
            // The gateway's own reply, folded so the pending set stays honest.
            "extension_ui_response" => text(record, "id")
                .filter(|id| self.pending_permissions.remove(*id))
                .map(|id| vec![LiveChange::PermissionResolved(id.to_string())])
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Track a new live message under a fresh provisional key.
    fn open(&mut self, message: &Value) -> String {
        self.minted += 1;
        let key = format!("{PROVISIONAL_PREFIX}{}", self.minted);
        if self.messages.len() >= MAX_LIVE_MESSAGES {
            self.messages.remove(0);
        }
        self.messages.push(LiveMessage {
            key: key.clone(),
            role: text(message, "role").unwrap_or_default().to_string(),
            timestamp: message
                .get("timestamp")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            items: Vec::new(),
        });
        key
    }

    /// Replace a message's rows wholesale and move it to `new_key`, keeping
    /// each surviving row's `seq`; emits the ids that disappeared, then the
    /// new rows.
    fn replace_rows(
        &mut self,
        key: &str,
        new_key: &str,
        mut rows: Vec<TimelineItem>,
    ) -> Vec<LiveChange> {
        let Some(at) = self.messages.iter().position(|m| m.key == key) else {
            return vec![];
        };
        let old = std::mem::take(&mut self.messages[at].items);
        for row in &mut rows {
            row.seq = match old.iter().find(|o| o.id == row.id) {
                Some(o) => o.seq,
                None => self.next_seq(),
            };
        }
        let removed: Vec<String> = old
            .iter()
            .filter(|o| !rows.iter().any(|r| r.id == o.id))
            .map(|o| o.id.clone())
            .collect();
        let message = &mut self.messages[at];
        message.key = new_key.to_string();
        message.items = rows.clone();
        let mut changes = Vec::new();
        if !removed.is_empty() {
            changes.push(LiveChange::Remove(removed));
        }
        if !rows.is_empty() {
            changes.push(LiveChange::Upsert(rows));
        }
        changes
    }

    fn on_update(&mut self, record: &Value) -> Vec<LiveChange> {
        let mut changes = Vec::new();
        if let Some(row) = record
            .get("assistantMessageEvent")
            .and_then(|event| self.on_stream_event(event))
        {
            changes.push(LiveChange::Upsert(vec![row]));
        }
        changes.extend(self.usage(record.get("usage")));
        changes
    }

    /// Apply one `assistantMessageEvent` to the streaming message's row for
    /// that content block; returns the row when it changed.
    fn on_stream_event(&mut self, event: &Value) -> Option<TimelineItem> {
        let kind = text(event, "type")?;
        let index = event.get("contentIndex").and_then(Value::as_u64)?;
        let tool = match kind {
            "toolcall_end" => Some(event.get("toolCall")?),
            _ => None,
        };
        let key = self.current.clone()?;
        let message = self.messages.iter_mut().find(|m| m.key == key)?;
        let id = format!("{key}:{index}");
        let at = match message.items.iter().position(|r| r.id == id) {
            Some(at) => at,
            None => {
                let part = match kind {
                    "text_delta" | "text_end" => AgentPart::Text {
                        text: String::new(),
                    },
                    "thinking_delta" | "thinking_end" => AgentPart::Reasoning {
                        text: String::new(),
                        duration_ms: None,
                    },
                    "toolcall_end" => {
                        AgentPart::Tool(tool_call("", "", Value::Null, ToolCallStatus::Pending))
                    }
                    _ => return None,
                };
                let mid = message_id(message.timestamp, &key);
                let mut row = item(
                    id,
                    mid,
                    TimelineRole::Assistant,
                    part,
                    index,
                    message.timestamp,
                );
                self.seq += 1;
                row.seq = self.seq;
                message.items.push(row);
                message.items.len() - 1
            }
        };
        let row = &mut message.items[at];
        match (&mut row.part, kind, tool) {
            (AgentPart::Text { text: body }, "text_delta", _)
            | (AgentPart::Reasoning { text: body, .. }, "thinking_delta", _) => {
                body.push_str(text(event, "delta").unwrap_or_default());
            }
            (AgentPart::Text { text: body }, "text_end", _)
            | (AgentPart::Reasoning { text: body, .. }, "thinking_end", _) => {
                *body = text(event, "content").unwrap_or_default().to_string();
            }
            (AgentPart::Tool(call), "toolcall_end", Some(tool)) => {
                *call = tool_call(
                    text(tool, "id").unwrap_or_default(),
                    text(tool, "name").unwrap_or("tool"),
                    tool.get("arguments").cloned().unwrap_or(Value::Null),
                    ToolCallStatus::Pending,
                );
                if let Some(outcome) = self.outcomes.get(&call.id) {
                    outcome.apply(call);
                }
            }
            _ => return None,
        }
        Some(row.clone())
    }

    fn on_message_end(&mut self, message: &Value) -> Vec<LiveChange> {
        match text(message, "role") {
            Some("assistant") => {
                let key = match self.current.take() {
                    Some(key) => key,
                    None => self.open(message),
                };
                let ts = message
                    .get("timestamp")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let rows = message_items(&key, ts, message, &self.outcomes);
                let mut changes = self.replace_rows(&key, &key, rows);
                changes.extend(self.usage(message.get("usage")));
                changes
            }
            Some("user") => {
                let key = self.open(message);
                let ts = message
                    .get("timestamp")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let rows = message_items(&key, ts, message, &self.outcomes);
                self.replace_rows(&key, &key, rows)
            }
            Some("toolResult") => {
                let Some(call_id) = text(message, "toolCallId") else {
                    return vec![];
                };
                let is_error = message.get("isError").and_then(Value::as_bool) == Some(true);
                self.record_outcome(call_id, ToolOutcome::finished(is_error, message))
            }
            _ => vec![],
        }
    }

    fn on_tool_execution(&mut self, record: &Value) -> Vec<LiveChange> {
        // Calls a tool makes itself have no row of their own.
        if record.get("parentToolCallId").is_some() {
            return vec![];
        }
        let Some(call_id) = text(record, "toolCallId") else {
            return vec![];
        };
        let outcome = if text(record, "type") == Some("tool_execution_start") {
            ToolOutcome {
                state: ToolCallStatus::Running,
                input: record.get("args").cloned(),
                result: None,
            }
        } else {
            let is_error = record.get("isError").and_then(Value::as_bool) == Some(true);
            let result = record.get("result").cloned().unwrap_or(Value::Null);
            let mut outcome = ToolOutcome::finished(is_error, &result);
            outcome.input = self.outcomes.get(call_id).and_then(|o| o.input.clone());
            outcome
        };
        self.record_outcome(call_id, outcome)
    }

    /// Remember what a tool call did and update its row when it is known.
    fn record_outcome(&mut self, call_id: &str, outcome: ToolOutcome) -> Vec<LiveChange> {
        let mut changed = None;
        for message in &mut self.messages {
            for row in &mut message.items {
                if let AgentPart::Tool(call) = &mut row.part {
                    if call.id == call_id {
                        let before = call.clone();
                        outcome.apply(call);
                        if *call != before {
                            changed = Some(row.clone());
                        }
                    }
                }
            }
        }
        self.outcomes.insert(call_id.to_string(), outcome);
        changed
            .map(|row| vec![LiveChange::Upsert(vec![row])])
            .unwrap_or_default()
    }

    /// A persisted entry: re-key the live message it records.
    fn on_entry(&mut self, entry: &Value) -> Vec<LiveChange> {
        if text(entry, "type") != Some("message") {
            return vec![];
        }
        let (Some(entry_id), Some(message)) = (text(entry, "id"), entry.get("message")) else {
            return vec![];
        };
        let role = text(message, "role").unwrap_or_default();
        let ts = message.get("timestamp").and_then(Value::as_u64);
        let Some(key) = self
            .messages
            .iter()
            .find(|m| {
                m.key.starts_with(PROVISIONAL_PREFIX) && m.role == role && Some(m.timestamp) == ts
            })
            .map(|m| m.key.clone())
        else {
            return vec![];
        };
        if self.current.as_deref() == Some(key.as_str()) {
            self.current = None;
        }
        let ms = text(entry, "timestamp")
            .and_then(ms_from_iso)
            .or(ts)
            .unwrap_or(0);
        let rows = message_items(entry_id, ms, message, &self.outcomes);
        // Every provisional id goes, even where the new id would collide with
        // nothing: the App must not keep a `live-` row beside the real one.
        let old_ids: Vec<String> = self
            .messages
            .iter_mut()
            .find(|m| m.key == key)
            .map(|m| m.items.drain(..).map(|r| r.id).collect())
            .unwrap_or_default();
        let mut changes = Vec::new();
        if !old_ids.is_empty() {
            changes.push(LiveChange::Remove(old_ids));
        }
        changes.extend(self.replace_rows(&key, entry_id, rows));
        changes
    }

    fn usage(&mut self, usage: Option<&Value>) -> Vec<LiveChange> {
        let Some(usage) = usage else {
            return vec![];
        };
        let count = |key: &str| usage.get(key).and_then(Value::as_u64);
        let total_tokens = count("totalTokens").unwrap_or(0);
        // Providers that do not stream usage report zeros until completion.
        if total_tokens == 0 || total_tokens == self.last_total_tokens {
            return vec![];
        }
        self.last_total_tokens = total_tokens;
        vec![LiveChange::Usage {
            tokens: TokensUsage {
                input: count("input").unwrap_or(0),
                output: count("output").unwrap_or(0),
                reasoning: count("reasoning"),
                cache_read: count("cacheRead"),
                cache_write: count("cacheWrite"),
            },
            cost: usage.pointer("/cost/total").and_then(Value::as_f64),
            total_tokens,
        }]
    }

    fn on_ui_request(&mut self, record: &Value) -> Vec<LiveChange> {
        let Some(id) = text(record, "id") else {
            return vec![];
        };
        let method = text(record, "method").unwrap_or_default();
        if !matches!(method, "select" | "confirm" | "input" | "editor") {
            // Fire-and-forget (`notify`, `setStatus`, ...): nothing to answer.
            return vec![];
        }
        if method == "confirm" && text(record, "title") == Some(PERMISSION_TITLE) {
            if let Some(request) = self.permission(id, text(record, "message")) {
                self.pending_permissions.insert(id.to_string());
                return vec![LiveChange::Permission(request)];
            }
        }
        // A dialog nobody will see, or our own with an unreadable payload:
        // cancelling makes `confirm` false, so the gate fails closed.
        vec![LiveChange::UiCancel(id.to_string())]
    }

    fn permission(&self, id: &str, message: Option<&str>) -> Option<PermissionRequest> {
        let payload: Value = serde_json::from_str(message?).ok()?;
        let tool = text(&payload, "tool")?;
        let input = payload.get("input").cloned().unwrap_or(Value::Null);
        let call_id = text(&payload, "toolCallId");
        let (prompt, resources) = permission_prompt(tool, &input);
        let source_message_id = call_id.and_then(|call_id| {
            self.messages.iter().find_map(|m| {
                m.items.iter().find_map(|row| match &row.part {
                    AgentPart::Tool(call) if call.id == call_id => Some(row.message_id.clone()),
                    _ => None,
                })
            })
        });
        Some(PermissionRequest {
            id: id.to_string(),
            asid: self.asid.clone(),
            action: tool.to_string(),
            resources,
            save: vec![],
            prompt,
            tool: Some(tool.to_string()),
            source_message_id,
            source_tool_call_id: call_id.map(str::to_string),
            metadata: Some(input),
            message: None,
            options: vec![
                PermissionOption {
                    index: 0,
                    label: "Allow".into(),
                    decision: PermissionDecision::Allow,
                },
                PermissionOption {
                    index: 1,
                    label: "Deny".into(),
                    decision: PermissionDecision::Deny,
                },
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `id kind[ state][ text]`, enough to pin what a row is without
    /// restating every field.
    fn row(item: &TimelineItem) -> String {
        let kind = match &item.part {
            AgentPart::Text { text } => format!("text {text}"),
            AgentPart::Reasoning { text, .. } => format!("reasoning {text}"),
            AgentPart::Tool(call) => format!("tool {} {:?}", call.name, call.state),
            AgentPart::Compaction { status, .. } => format!("compaction {status:?}"),
            AgentPart::ModelSwitched { model, .. } => format!("model {}", model.model_id),
            other => format!("{other:?}"),
        };
        format!("{} {kind}", item.id)
    }

    fn change(change: &LiveChange) -> String {
        match change {
            LiveChange::Status(status) => format!("status {status:?}"),
            LiveChange::Upsert(rows) => {
                format!(
                    "upsert [{}]",
                    rows.iter().map(row).collect::<Vec<_>>().join(", ")
                )
            }
            LiveChange::Remove(ids) => format!("remove {ids:?}"),
            LiveChange::Usage { total_tokens, .. } => format!("usage {total_tokens}"),
            other => format!("{other:?}"),
        }
    }

    fn assistant(content: Value) -> Value {
        json!({"role": "assistant", "content": content, "provider": "demo", "model": "demo-1",
               "usage": {"input": 10, "output": 5, "totalTokens": 15, "cost": {"total": 0.01}},
               "stopReason": "toolUse", "timestamp": 1_700_000_000_100u64})
    }

    #[test]
    fn session_entries_map_to_timeline_rows() {
        let at = |s: u32| format!("2026-01-01T00:00:{s:02}.000Z");
        let entries = vec![
            json!({"type": "message", "id": "e1", "parentId": null, "timestamp": at(1),
                   "message": {"role": "user", "content": [{"type": "text", "text": "list files"}],
                               "timestamp": 1_700_000_000_000u64}}),
            json!({"type": "message", "id": "e2", "parentId": "e1", "timestamp": at(2),
                   "message": assistant(json!([
                       {"type": "thinking", "thinking": "look first"},
                       {"type": "text", "text": "Listing."},
                       {"type": "toolCall", "id": "call-1", "name": "bash",
                        "arguments": {"command": "ls"}}]))}),
            json!({"type": "message", "id": "e3", "parentId": "e2", "timestamp": at(3),
                   "message": {"role": "toolResult", "toolCallId": "call-1", "toolName": "bash",
                               "content": [{"type": "text", "text": "README.md"}],
                               "isError": false, "timestamp": 1_700_000_000_200u64}}),
            json!({"type": "thinking_level_change", "id": "e4", "parentId": "e3",
                   "timestamp": at(4), "thinkingLevel": "high"}),
            json!({"type": "compaction", "id": "e5", "parentId": "e4", "timestamp": at(5),
                   "summary": "Listed files.", "firstKeptEntryId": "e3", "tokensBefore": 900}),
            json!({"type": "model_change", "id": "e6", "parentId": "e5", "timestamp": at(6),
                   "provider": "demo", "modelId": "demo-2"}),
        ];
        let items = timeline_from_entries(&entries);
        let expected = [
            "e1 text list files",
            "e2:0 reasoning look first",
            "e2:1 text Listing.",
            "e2:2 tool bash Completed",
            "e5 compaction Completed",
            "e6 model demo-2",
        ];
        assert_eq!(items.iter().map(row).collect::<Vec<_>>(), expected);
        let AgentPart::Tool(call) = &items[3].part else {
            unreachable!()
        };
        assert_eq!(call.output, Some(json!("README.md")));
        // Rows sort by message id in session order although entry ids are random.
        let mut sorted = items.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        assert_eq!(sorted, items);
        assert!(items.iter().enumerate().all(|(i, it)| it.seq == i as u64));
    }

    #[test]
    fn live_stream_folds_and_rekeys_to_the_persisted_entry() {
        let final_message = assistant(json!([
            {"type": "text", "text": "Hello"},
            {"type": "toolCall", "id": "call-1", "name": "bash", "arguments": {"command": "ls"}}]));
        let steps: Vec<(Value, Vec<&str>)> = vec![
            (json!({"type": "agent_start"}), vec!["status Busy"]),
            (
                json!({"type": "message_start", "message":
                    {"role": "assistant", "content": [], "stopReason": "pending",
                     "timestamp": 1_700_000_000_100u64}}),
                vec![],
            ),
            (
                json!({"type": "message_update", "assistantMessageEvent":
                    {"type": "text_delta", "contentIndex": 0, "delta": "Hel"}}),
                vec!["upsert [live-1:0 text Hel]"],
            ),
            (
                json!({"type": "message_update", "assistantMessageEvent":
                    {"type": "text_delta", "contentIndex": 0, "delta": "lo"}}),
                vec!["upsert [live-1:0 text Hello]"],
            ),
            (
                json!({"type": "message_update", "assistantMessageEvent":
                    {"type": "toolcall_end", "contentIndex": 1, "toolCall":
                        {"type": "toolCall", "id": "call-1", "name": "bash",
                         "arguments": {"command": "ls"}}}}),
                vec!["upsert [live-1:1 tool bash Pending]"],
            ),
            (
                json!({"type": "tool_execution_start", "toolCallId": "call-1",
                       "toolName": "bash", "args": {"command": "ls"}}),
                vec!["upsert [live-1:1 tool bash Running]"],
            ),
            (
                json!({"type": "tool_execution_end", "toolCallId": "call-1", "toolName": "bash",
                       "result": {"content": [{"type": "text", "text": "README.md"}]},
                       "isError": false}),
                vec!["upsert [live-1:1 tool bash Completed]"],
            ),
            (
                json!({"type": "message_end", "message": final_message}),
                vec![
                    "upsert [live-1:0 text Hello, live-1:1 tool bash Completed]",
                    "usage 15",
                ],
            ),
            (
                json!({"type": "entry_appended", "entry": {"type": "message", "id": "e9",
                       "parentId": "e8", "timestamp": "2026-01-01T00:00:09.000Z",
                       "message": final_message}}),
                vec![
                    r#"remove ["live-1:0", "live-1:1"]"#,
                    "upsert [e9:0 text Hello, e9:1 tool bash Completed]",
                ],
            ),
            (
                json!({"type": "agent_settled", "aborted": false}),
                vec!["status Idle"],
            ),
        ];
        let mut folder = LiveFolder::new("s1".into()).with_history_len(4);
        let mut seqs = Vec::new();
        for (record, expected) in steps {
            let changes = folder.fold(&record);
            assert_eq!(
                changes.iter().map(change).collect::<Vec<_>>(),
                expected,
                "{record}"
            );
            for c in &changes {
                if let LiveChange::Upsert(rows) = c {
                    seqs.extend(rows.iter().map(|r| r.seq));
                }
            }
        }
        assert!(seqs.iter().all(|&s| s > 4), "{seqs:?}");
    }

    #[test]
    fn permission_confirm_becomes_a_permission_request() {
        let cases = [
            (
                "bash",
                json!({"command": "rm -rf build"}),
                "bash: rm -rf build",
                "rm -rf build",
            ),
            (
                "edit",
                json!({"path": "src/main.rs"}),
                "edit src/main.rs",
                "src/main.rs",
            ),
        ];
        for (tool, input, prompt, resource) in cases {
            let mut folder = LiveFolder::new("s1".into());
            let payload = json!({"tool": tool, "toolCallId": "call-7", "input": input});
            let request = json!({"type": "extension_ui_request", "id": "ui-1",
                                 "method": "confirm", "title": PERMISSION_TITLE,
                                 "message": payload.to_string()});
            let changes = folder.fold(&request);
            let [LiveChange::Permission(req)] = changes.as_slice() else {
                panic!("{changes:?}")
            };
            assert_eq!(
                (req.id.as_str(), req.asid.0.as_str(), req.action.as_str()),
                ("ui-1", "s1", tool)
            );
            assert_eq!(req.prompt, prompt);
            assert_eq!(req.resources, [resource]);
            assert_eq!(req.tool.as_deref(), Some(tool));
            assert_eq!(req.source_tool_call_id.as_deref(), Some("call-7"));
            assert_eq!(req.metadata.as_ref(), Some(&input));
            let decisions: Vec<_> = req.options.iter().map(|o| (o.index, o.decision)).collect();
            assert_eq!(
                decisions,
                [
                    (0, PermissionDecision::Allow),
                    (1, PermissionDecision::Deny)
                ]
            );
            let resolved = folder.fold(&json!({"type": "extension_ui_response", "id": "ui-1",
                                               "confirmed": true}));
            assert_eq!(resolved, [LiveChange::PermissionResolved("ui-1".into())]);
        }
    }
}
