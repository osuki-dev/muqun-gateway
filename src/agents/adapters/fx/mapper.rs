//! ACP data is projected into the existing agent contract, never exposed as a
//! second App protocol. Native ids remain opaque within one stream. A native
//! load generates new ids and replaces its prior snapshot atomically.
use std::collections::HashMap;

use serde_json::Value;

use crate::agents::domain::*;
use crate::agents::ports::agent::AgentError;

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn session(id: &str, directory: &str, value: &Value) -> AgentSessionInfo {
    AgentSessionInfo {
        asid: id.into(),
        agent_id: String::new(),
        backend_session_id: id.into(),
        title: value["title"].as_str().unwrap_or_default().into(),
        mode: value
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: selected_model(value),
        status: AgentSessionStatus::Idle,
        runtime: None,
        context_usage: None,
        git_info: None,
        agent_name: None,
        agent_role: None,
        directory: Some(directory.into()),
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
        updated_ms: value["updatedAt"]
            .as_str()
            .and_then(crate::agents::adapters::t3::client::ms_from_iso)
            .unwrap_or(0),
    }
}

pub(super) fn config_option<'a>(value: &'a Value, id: &str) -> Option<&'a Value> {
    value["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"].as_str() == Some(id))
}

pub(super) fn selected_model(value: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider_id: config_option(value, "provider")?["currentValue"]
            .as_str()?
            .into(),
        model_id: config_option(value, "model")?["currentValue"]
            .as_str()?
            .into(),
        variant: config_option(value, "effort")
            .and_then(|option| option["currentValue"].as_str())
            .map(str::to_owned),
    })
}

pub(super) fn catalog(value: &Value) -> AgentCatalog {
    let mut catalog = AgentCatalog::default();
    let selected = selected_model(value);
    if let Some(models) =
        config_option(value, "model").and_then(|option| option["options"].as_array())
    {
        catalog.models = models
            .iter()
            .filter_map(|model| {
                let id = model["value"].as_str()?;
                Some(ModelInfo {
                    id: id.into(),
                    name: model["name"].as_str().unwrap_or(id).into(),
                    provider_id: selected
                        .as_ref()
                        .map(|model| model.provider_id.clone())
                        .unwrap_or_else(|| "fx".into()),
                    family: None,
                    limit: None,
                    variants: if selected
                        .as_ref()
                        .is_some_and(|selected| selected.model_id == id)
                    {
                        config_option(value, "effort")
                            .and_then(|option| option["options"].as_array())
                            .map(|options| {
                                options
                                    .iter()
                                    .filter_map(|option| {
                                        let value = option["value"].as_str()?;
                                        Some(ModelVariantInfo {
                                            id: value.into(),
                                            reasoning_effort: Some(value.into()),
                                        })
                                    })
                                    .collect()
                            })
                    } else {
                        None
                    },
                    cost: None,
                    enabled: true,
                    status: None,
                })
            })
            .collect();
    }
    if let Some(modes) = value
        .pointer("/modes/availableModes")
        .and_then(Value::as_array)
    {
        catalog.modes = modes
            .iter()
            .filter_map(|mode| {
                let id = mode["id"].as_str()?;
                Some(ModeInfo {
                    id: id.into(),
                    name: mode["name"].as_str().unwrap_or(id).into(),
                    model: None,
                    description: mode["description"].as_str().map(str::to_owned),
                    mode: None,
                    color: None,
                    hidden: false,
                })
            })
            .collect();
    }
    catalog.defaults.model = selected;
    catalog.defaults.mode = value
        .pointer("/modes/currentModeId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    catalog
}

pub(super) struct Permission {
    pub request: PermissionRequest,
    pub rpc_id: Value,
    pub options: Vec<(PermissionDecision, String)>,
}

pub(super) fn permission(rpc_id: Value, params: &Value) -> Result<Permission, AgentError> {
    let session = params["sessionId"]
        .as_str()
        .ok_or_else(|| AgentError::Protocol("FX permission missing sessionId".into()))?;
    let tool = &params["toolCall"];
    let mut options = Vec::new();
    let mut mapped = Vec::new();
    for option in params["options"].as_array().into_iter().flatten() {
        let decision = match option["kind"].as_str() {
            Some("allow_once") => PermissionDecision::Allow,
            Some("allow_always") => PermissionDecision::AllowAlways,
            Some("reject_once") => PermissionDecision::Deny,
            // Reject-always has broader side effects than the App's deny.
            _ => continue,
        };
        let Some(id) = option["optionId"].as_str() else {
            continue;
        };
        options.push((decision, id.into()));
        mapped.push(PermissionOption {
            index: mapped.len(),
            label: option["name"].as_str().unwrap_or(id).into(),
            decision,
        });
    }
    // JSON-RPC ids may be reused after a response. A stale App reply must
    // never authorize a later request that happens to reuse the transport id.
    let id = format!("fx:{}", uuid::Uuid::new_v4());
    let name = tool["name"]
        .as_str()
        .or_else(|| tool["kind"].as_str())
        .unwrap_or("tool");
    let request = PermissionRequest {
        id,
        asid: session.into(),
        action: name.into(),
        resources: Vec::new(),
        save: Vec::new(),
        prompt: tool["title"].as_str().unwrap_or(name).into(),
        tool: Some(name.into()),
        source_message_id: None,
        source_tool_call_id: tool["toolCallId"].as_str().map(str::to_owned),
        metadata: Some(params.clone()),
        message: None,
        options: mapped,
    };
    Ok(Permission {
        request,
        rpc_id,
        options,
    })
}

#[derive(Default)]
pub(super) struct Projection {
    pub rows: HashMap<String, TimelineItem>,
    observed_bytes: usize,
    groups: HashMap<String, String>,
    next_group: u64,
    next_row: u64,
    current: Option<String>,
    thought: Option<String>,
    tool_groups: HashMap<String, String>,
}

impl Projection {
    fn group(&mut self, native: &str) -> String {
        if let Some(group) = self.groups.get(native) {
            return group.clone();
        }
        self.next_group += 1;
        let group = format!("{:020}:{native}", self.next_group);
        self.groups.insert(native.into(), group.clone());
        group
    }

    pub fn can_submit(&self, text: &str) -> Result<(), AgentError> {
        if self
            .observed_bytes
            .saturating_add(text.len())
            .saturating_add(512)
            > 32 * 1024 * 1024
            || self.rows.len() >= 5000
        {
            return Err(AgentError::NotAvailable("FX transcript exceeds the Gateway history budget; open this session in the FX terminal".into()));
        }
        Ok(())
    }

    /// The ordinary live ACP stream omits user echoes. This row records only
    /// the Gateway's flushed submission, and is replaced by native history on load.
    pub fn submitted(&mut self, text: &str) -> TimelineItem {
        self.begin_turn();
        self.observed_bytes += text.len() + 512;
        let native = format!("fx-submission:{}", uuid::Uuid::new_v4());
        let group = self.group(&native);
        self.row(
            native,
            group,
            TimelineRole::User,
            AgentPart::Text { text: text.into() },
        )
    }

    pub fn begin_turn(&mut self) {
        self.current = None;
        self.thought = None;
    }

    fn assistant_group(&mut self) -> String {
        if let Some(group) = &self.current {
            return group.clone();
        }
        let native = format!("fx-turn-{}", self.next_group + 1);
        let group = self.group(&native);
        self.current = Some(group.clone());
        group
    }

    fn row(
        &mut self,
        id: String,
        group: String,
        role: TimelineRole,
        part: AgentPart,
    ) -> TimelineItem {
        let ordinal = if let Some(row) = self.rows.get(&id) {
            row.ordinal
        } else {
            self.next_row += 1;
            self.next_row
        };
        let row = TimelineItem {
            id: id.clone(),
            message_id: group,
            role,
            part,
            seq: ordinal,
            updated_ms: now_ms(),
            ordinal,
            attachments: None,
            image_assets: None,
            generation: None,
        };
        self.rows.insert(id, row.clone());
        row
    }

    pub fn update(&mut self, update: &Value) -> Result<Option<TimelineItem>, AgentError> {
        // A whole-history ACP replay has no pagination. Refuse a transcript
        // we cannot retain completely instead of silently claiming exhaustion.
        let bytes = serde_json::to_vec(update)
            .map_err(|_| AgentError::Protocol("Invalid FX update".into()))?
            .len();
        if self.observed_bytes.saturating_add(bytes) > 32 * 1024 * 1024 || self.rows.len() >= 5000 {
            return Err(AgentError::NotAvailable("FX transcript exceeds the Gateway history budget (5000 rows / 32 MiB); open this session in the FX terminal".into()));
        }
        self.observed_bytes += bytes;
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        match kind {
            "agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk" => {
                let Some(text) = update.pointer("/content/text").and_then(Value::as_str) else {
                    return Ok(None);
                };
                let thought = kind == "agent_thought_chunk";
                let user = kind == "user_message_chunk";
                let (id, group) = if thought {
                    let group = self.assistant_group();
                    let id = self
                        .thought
                        .get_or_insert_with(|| format!("{group}:reasoning"))
                        .clone();
                    (id, group)
                } else {
                    let native = update["messageId"].as_str().ok_or_else(|| {
                        AgentError::Protocol("FX text chunk missing stable messageId".into())
                    })?;
                    let group = self.group(native);
                    if user {
                        self.begin_turn();
                    } else {
                        self.current = Some(group.clone());
                    }
                    (format!("fx-message:{native}:text"), group)
                };
                let mut full = self
                    .rows
                    .get(&id)
                    .and_then(|row| match &row.part {
                        AgentPart::Text { text } | AgentPart::Reasoning { text, .. } => {
                            Some(text.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                full.push_str(text);
                let part = if thought {
                    AgentPart::Reasoning {
                        text: full,
                        duration_ms: None,
                    }
                } else {
                    AgentPart::Text { text: full }
                };
                Ok(Some(self.row(
                    id,
                    group,
                    if user {
                        TimelineRole::User
                    } else {
                        TimelineRole::Assistant
                    },
                    part,
                )))
            }
            "tool_call" | "tool_call_update" => {
                let native = update["toolCallId"]
                    .as_str()
                    .ok_or_else(|| AgentError::Protocol("FX tool missing toolCallId".into()))?;
                let id = format!("fx-tool:{native}");
                let mut call = self
                    .rows
                    .get(&id)
                    .and_then(|row| match &row.part {
                        AgentPart::Tool(call) => Some(call.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| ToolCall {
                        id: native.into(),
                        name: String::new(),
                        title: None,
                        input: Value::Null,
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
                        time: ToolTime::default(),
                    });
                if let Some(name) = update["name"].as_str().or_else(|| update["kind"].as_str()) {
                    call.name = name.into();
                }
                if let Some(title) = update["title"].as_str() {
                    call.title = Some(title.into());
                }
                if let Some(input) = update.get("rawInput") {
                    call.input = input.clone();
                }
                if let Some(content) = update.get("content") {
                    let texts: Vec<&str> = content
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|part| part.pointer("/content/text").and_then(Value::as_str))
                        .collect();
                    call.output = Some(Value::String(texts.join("\n")));
                    call.content = Some(content.clone());
                }
                if let Some(output) = update.get("rawOutput") {
                    call.output = Some(output.clone());
                }
                if let Some(metadata) = update.get("_meta") {
                    call.metadata = Some(metadata.clone());
                }
                if let Some(status) = update["status"].as_str() {
                    let status = match status {
                        "pending" => ToolCallStatus::Pending,
                        "in_progress" => ToolCallStatus::Running,
                        "completed" => ToolCallStatus::Completed,
                        "failed" => ToolCallStatus::Failed,
                        _ => {
                            return Err(AgentError::Protocol(format!(
                                "unknown FX tool status {status}"
                            )))
                        }
                    };
                    if !matches!(
                        call.state,
                        ToolCallStatus::Completed | ToolCallStatus::Failed
                    ) || matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
                    {
                        call.set_state(status);
                    }
                }
                let group = if let Some(group) = self.tool_groups.get(native) {
                    group.clone()
                } else {
                    let group = self.assistant_group();
                    self.tool_groups.insert(native.into(), group.clone());
                    group
                };
                Ok(Some(self.row(
                    id,
                    group,
                    TimelineRole::Assistant,
                    AgentPart::Tool(call),
                )))
            }
            _ => Ok(None),
        }
    }

    pub fn timeline(&self) -> Vec<TimelineItem> {
        let mut rows: Vec<_> = self.rows.values().cloned().collect();
        rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        rows
    }
}
