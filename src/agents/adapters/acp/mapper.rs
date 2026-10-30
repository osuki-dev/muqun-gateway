//! ACP data is projected into the existing agent contract, never exposed as a
//! second App protocol. Native ids remain opaque within one stream. A native
//! load generates new ids and replaces its prior snapshot atomically.
use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::agents::domain::*;
use crate::agents::ports::agent::AgentError;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn session(id: &str, directory: &str, value: &Value) -> AgentSessionInfo {
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

pub(crate) fn config_option<'a>(value: &'a Value, id: &str) -> Option<&'a Value> {
    value["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["id"].as_str() == Some(id))
}

pub(crate) fn selected_model(value: &Value) -> Option<ModelRef> {
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

/// ACP allows providerless model catalogs; keep native model ids opaque.
pub(crate) fn selected_model_for(value: &Value, provider: &str) -> Option<ModelRef> {
    if provider == "fx" {
        return selected_model(value);
    }
    Some(ModelRef {
        provider_id: provider.into(),
        model_id: config_option(value, "model")
            .and_then(|option| option["currentValue"].as_str())
            .or_else(|| {
                value
                    .pointer("/models/currentModelId")
                    .and_then(Value::as_str)
            })?
            .into(),
        variant: config_option(value, "reasoning_effort")
            .and_then(|option| option["currentValue"].as_str())
            .map(str::to_owned),
    })
}

pub(crate) fn catalog_for(value: &Value, provider: &str) -> AgentCatalog {
    if provider == "fx" {
        catalog(value)
    } else {
        AgentCatalog::default()
    }
}

pub(crate) fn catalog(value: &Value) -> AgentCatalog {
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

pub(crate) struct Permission {
    pub request: PermissionRequest,
    pub rpc_id: Value,
    pub options: Vec<(PermissionDecision, String)>,
}

pub(crate) fn permission(rpc_id: Value, params: &Value) -> Result<Permission, AgentError> {
    let session = params["sessionId"]
        .as_str()
        .ok_or_else(|| AgentError::Protocol("ACP permission missing sessionId".into()))?;
    let tool = &params["toolCall"];
    let mut options = Vec::new();
    let mut mapped = Vec::new();
    let mut option_ids = HashSet::new();
    for option in params["options"].as_array().into_iter().flatten() {
        let id = option["optionId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AgentError::Protocol("ACP permission option has no opaque id".into()))?;
        if !option_ids.insert(id) {
            return Err(AgentError::Protocol(
                "ACP permission option ids are ambiguous".into(),
            ));
        }
        let decision = match option["kind"].as_str() {
            Some("allow_once") => PermissionDecision::Allow,
            Some("allow_always") => PermissionDecision::AllowAlways,
            Some("reject_once") => PermissionDecision::Deny,
            // Reject-always has broader side effects than the App's deny.
            _ => continue,
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
    let id = format!("acp:{}", uuid::Uuid::new_v4());
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

pub(crate) struct Projection {
    require_message_id: bool,
    anonymous: Option<(String, String)>,
    submission_echo: Option<(String, String)>,
    pub rows: HashMap<String, TimelineItem>,
    observed_bytes: usize,
    groups: HashMap<String, String>,
    next_group: u64,
    next_row: u64,
    current: Option<String>,
    thought: Option<String>,
    tool_groups: HashMap<String, String>,
}

impl Default for Projection {
    fn default() -> Self {
        Self::new(true)
    }
}
impl Projection {
    pub fn new(require_message_id: bool) -> Self {
        Self {
            require_message_id,
            anonymous: None,
            submission_echo: None,
            rows: HashMap::new(),
            observed_bytes: 0,
            groups: HashMap::new(),
            next_group: 0,
            next_row: 0,
            current: None,
            thought: None,
            tool_groups: HashMap::new(),
        }
    }

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
            return Err(AgentError::NotAvailable("ACP transcript exceeds the Gateway history budget; open this session in the agent's terminal".into()));
        }
        Ok(())
    }

    /// The ordinary live ACP stream omits user echoes. This row records only
    /// the Gateway's flushed submission, and is replaced by native history on load.
    pub fn submitted(&mut self, text: &str) -> TimelineItem {
        self.begin_turn();
        if !self.require_message_id {
            self.submission_echo = Some((text.into(), String::new()));
        }
        self.observed_bytes += text.len() + 512;
        let native = format!("acp-submission:{}", uuid::Uuid::new_v4());
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
        self.anonymous = None;
    }

    fn assistant_group(&mut self) -> String {
        if let Some(group) = &self.current {
            return group.clone();
        }
        let native = format!("acp-turn-{}", self.next_group + 1);
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
            .map_err(|_| AgentError::Protocol("Invalid ACP update".into()))?
            .len();
        if self.observed_bytes.saturating_add(bytes) > 32 * 1024 * 1024 || self.rows.len() >= 5000 {
            return Err(AgentError::NotAvailable("ACP transcript exceeds the Gateway history budget (5000 rows / 32 MiB); open this session in the agent's terminal".into()));
        }
        self.observed_bytes += bytes;
        let kind = update["sessionUpdate"].as_str().unwrap_or_default();
        match kind {
            "agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk" => {
                // A role boundary also ends an anonymous segment when the new
                // chunk has a native id or carries unsupported non-text content.
                if self
                    .anonymous
                    .as_ref()
                    .is_some_and(|(previous, _)| previous != kind)
                {
                    self.anonymous = None;
                }
                let thought = kind == "agent_thought_chunk";
                let user = kind == "user_message_chunk";
                if user {
                    self.current = None;
                    self.thought = None;
                }
                let Some(text) = update.pointer("/content/text").and_then(Value::as_str) else {
                    return Ok(None);
                };
                let (id, group) = if thought {
                    self.anonymous = None;
                    let group = self.assistant_group();
                    let id = self
                        .thought
                        .get_or_insert_with(|| format!("{group}:reasoning"))
                        .clone();
                    (id, group)
                } else {
                    if user && !self.require_message_id {
                        if let Some((expected, seen)) = &mut self.submission_echo {
                            seen.push_str(text);
                            if expected.starts_with(seen.as_str()) {
                                return Ok(None);
                            }
                            return Err(AgentError::Protocol("ACP user echo differs from the submitted prompt; refresh before sending again".into()));
                        }
                    }
                    let native = match update["messageId"].as_str() {
                        Some(native) => {
                            self.anonymous = None;
                            native.to_owned()
                        }
                        None if self.require_message_id => {
                            return Err(AgentError::Protocol(
                                "ACP text chunk missing stable messageId".into(),
                            ))
                        }
                        None => match &self.anonymous {
                            Some((previous, id)) if previous == kind => id.clone(),
                            _ => {
                                let id = format!("acp-stream-{}", self.next_row + 1);
                                self.anonymous = Some((kind.into(), id.clone()));
                                id
                            }
                        },
                    };
                    let group = self.group(&native);
                    if user {
                        self.current = None;
                        self.thought = None;
                    } else {
                        self.current = Some(group.clone());
                    }
                    (format!("acp-message:{native}:text"), group)
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
                self.anonymous = None;
                let native = update["toolCallId"]
                    .as_str()
                    .ok_or_else(|| AgentError::Protocol("ACP tool missing toolCallId".into()))?;
                let id = format!("acp-tool:{native}");
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
                                "unknown ACP tool status {status}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn idless_stream_merges_chunks_but_splits_at_tools_and_turns() {
        let mut projection = Projection::new(false);
        projection.submitted("question");
        for update in [
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"One"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" two"}}),
            json!({"sessionUpdate":"tool_call","toolCallId":"t","status":"in_progress","kind":"execute"}),
            json!({"sessionUpdate":"tool_call_update","toolCallId":"t","status":"completed","rawOutput":"result"}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Three"}}),
        ] {
            projection.update(&update).unwrap();
        }
        projection.submitted("follow up");
        projection.update(&json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Four"}})).unwrap();
        let rows = projection.timeline();
        assert_eq!(rows.len(), 6);
        assert!(matches!(&rows[1].part, AgentPart::Text{text} if text == "One two"));
        assert!(
            matches!(&rows[2].part, AgentPart::Tool(tool) if tool.status == ToolCallStatus::Completed && tool.output == Some(json!("result")))
        );
        assert!(matches!(&rows[3].part, AgentPart::Text{text} if text == "Three"));
        assert_ne!(rows[1].id, rows[3].id);
        assert_ne!(rows[3].message_id, rows[5].message_id);
    }

    #[test]
    fn live_user_echo_reconciles_one_flushed_submission_without_duplicate_rows() {
        let mut projection = Projection::new(false);
        let submitted = projection.submitted("hello world");
        for text in ["hello", " world"] {
            assert!(projection.update(&json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":text}})).unwrap().is_none());
        }
        let rows = projection.timeline();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, submitted.id);
        assert!(matches!(&rows[0].part, AgentPart::Text{text} if text == "hello world"));
        assert!(projection.update(&json!({"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"unexpected"}})).is_err());
    }

    #[test]
    fn idless_history_uses_role_boundaries_and_native_snapshot_replacement() {
        let replay = || {
            let mut projection = Projection::new(false);
            for (kind, text) in [
                ("user_message_chunk", "Q"),
                ("agent_message_chunk", "A"),
                ("agent_message_chunk", "B"),
                ("user_message_chunk", "Next"),
            ] {
                projection
                    .update(&json!({"sessionUpdate":kind,"content":{"type":"text","text":text}}))
                    .unwrap();
            }
            projection.timeline()
        };
        let rows = replay();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].role, TimelineRole::User);
        assert!(matches!(&rows[1].part, AgentPart::Text{text} if text == "AB"));
        assert_eq!(
            rows.iter().map(|row| &row.id).collect::<Vec<_>>(),
            replay().iter().map(|row| &row.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn native_and_non_text_role_boundaries_cannot_reuse_anonymous_assistant_rows() {
        let mut projection = Projection::new(false);
        for update in [
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"before"}}),
            json!({"sessionUpdate":"user_message_chunk","messageId":"native-user","content":{"type":"text","text":"question"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after"}}),
            json!({"sessionUpdate":"user_message_chunk","content":{"type":"image","data":"opaque"}}),
            json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"last"}}),
        ] {
            projection.update(&update).unwrap();
        }
        let rows = projection.timeline();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[1].role, TimelineRole::User);
        let texts: Vec<_> = rows
            .iter()
            .filter_map(|row| match &row.part {
                AgentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["before", "question", "after", "last"]);
    }

    #[test]
    fn permission_option_ids_must_be_nonempty_and_unique_across_all_decisions() {
        for options in [
            json!([{"optionId":"same","kind":"allow_once"},{"optionId":"same","kind":"reject_once"}]),
            json!([{"optionId":"same","kind":"allow_once"},{"optionId":"same","kind":"reject_always"}]),
            json!([{"optionId":"","kind":"allow_once"}]),
            json!([{"kind":"allow_once"}]),
        ] {
            assert!(permission(
                json!(1),
                &json!({"sessionId":"s","toolCall":{"toolCallId":"t"},"options":options})
            )
            .is_err());
        }
        assert!(permission(json!(1), &json!({"sessionId":"s","toolCall":{"toolCallId":"t"},"options":[{"optionId":"allow-opaque","kind":"allow_once"},{"optionId":"deny-opaque","kind":"reject_once"}]})).is_ok());
    }

    #[test]
    fn providerless_model_metadata_does_not_enable_unverified_selection() {
        let value = json!({"configOptions":[{"id":"model","currentValue":"native-model"},{"id":"reasoning_effort","currentValue":"xhigh"}]});
        let model = selected_model_for(&value, "qoder").unwrap();
        assert_eq!(model.provider_id, "qoder");
        assert_eq!(model.model_id, "native-model");
        assert_eq!(model.variant.as_deref(), Some("xhigh"));
        let catalog = catalog_for(&value, "qoder");
        assert!(catalog.models.is_empty());
        assert!(catalog.modes.is_empty());
        assert!(catalog.defaults.model.is_none());
        assert!(catalog.defaults.mode.is_none());
    }
}
