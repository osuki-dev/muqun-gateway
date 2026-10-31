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
    selected_model_for(value, "fx")
}

pub(crate) fn effort_key(provider: &str) -> &'static str {
    match provider {
        "fx" => "effort",
        "antigravity" => "thought_level",
        _ => "reasoning_effort",
    }
}

/// ACP model ids are opaque. FX names its configured native provider; other
/// agents expose a providerless catalog within their own Gateway namespace.
pub(crate) fn selected_model_for(value: &Value, provider: &str) -> Option<ModelRef> {
    let provider_id = if provider == "fx" {
        config_option(value, "provider")?["currentValue"].as_str()?
    } else {
        provider
    };
    Some(ModelRef {
        provider_id: provider_id.into(),
        model_id: config_option(value, "model")
            .and_then(|option| option["currentValue"].as_str())
            .or_else(|| {
                value
                    .pointer("/models/currentModelId")
                    .and_then(Value::as_str)
            })?
            .into(),
        variant: config_option(value, effort_key(provider))
            .and_then(|option| option["currentValue"].as_str())
            .map(str::to_owned),
    })
}

fn select_options(option: &Value) -> Vec<&Value> {
    let mut values = Vec::new();
    for entry in option["options"].as_array().into_iter().flatten() {
        if let Some(group) = entry["options"].as_array() {
            values.extend(group);
        } else {
            values.push(entry);
        }
    }
    values
}

/// A cold CLI catalog may name a model while ACP exposes its opaque value.
/// Resolve only an exact native id or one unique native display name.
pub(crate) fn resolve_model_value(value: &Value, wanted: &str) -> Result<String, AgentError> {
    let options = config_option(value, "model")
        .map(select_options)
        .unwrap_or_default();
    if options
        .iter()
        .any(|option| option["value"].as_str() == Some(wanted))
    {
        return Ok(wanted.into());
    }
    let choices: std::collections::HashSet<_> = options
        .iter()
        .filter(|option| option["name"].as_str() == Some(wanted))
        .filter_map(|option| option["value"].as_str())
        .filter(|id| !id.is_empty())
        .collect();
    if choices.len() == 1 {
        return Ok(choices.into_iter().next().unwrap().into());
    }
    Err(AgentError::InvalidRequest(
        "model choice is not an unambiguous native option".into(),
    ))
}

pub(crate) fn offers_config_value(value: &Value, id: &str, wanted: &str) -> bool {
    config_option(value, id).is_some_and(|option| {
        select_options(option)
            .iter()
            .any(|option| option["value"].as_str() == Some(wanted))
    })
}

pub(crate) fn catalog_for(value: &Value, provider: &str) -> AgentCatalog {
    let selected = selected_model_for(value, provider);
    let provider_id = selected
        .as_ref()
        .map(|model| model.provider_id.as_str())
        .unwrap_or(provider);
    let mut models = std::collections::BTreeMap::new();
    // Antigravity 1.4 still lists 1.3's effort-suffixed ids here; its model
    // and thinking options are the real choices.
    if let Some(available) = value
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .filter(|_| provider != "antigravity")
    {
        for native in available {
            let Some(id) = native["modelId"].as_str() else {
                continue;
            };
            models.insert(
                id.to_owned(),
                ModelInfo {
                    id: id.into(),
                    name: native["name"].as_str().unwrap_or(id).into(),
                    provider_id: provider_id.into(),
                    family: None,
                    limit: None,
                    variants: None,
                    cost: None,
                    enabled: true,
                    status: None,
                },
            );
        }
    }
    if let Some(option) = config_option(value, "model") {
        for native in select_options(option) {
            let Some(id) = native["value"].as_str() else {
                continue;
            };
            models.insert(
                id.to_owned(),
                ModelInfo {
                    id: id.into(),
                    name: native["name"].as_str().unwrap_or(id).into(),
                    provider_id: provider_id.into(),
                    family: None,
                    limit: None,
                    variants: None,
                    cost: None,
                    enabled: true,
                    status: None,
                },
            );
        }
    }
    if let Some(current) = &selected {
        if let Some(model) = models.get_mut(&current.model_id) {
            model.variants = config_option(value, effort_key(provider)).map(|option| {
                select_options(option)
                    .into_iter()
                    .filter_map(|native| {
                        let id = native["value"].as_str()?;
                        Some(ModelVariantInfo {
                            id: id.into(),
                            reasoning_effort: Some(id.into()),
                        })
                    })
                    .collect()
            });
        }
    }
    // Neither ACP provider's permission-mode controls are session-local model
    // choices. Their modes must not leak into App new-session defaults.
    AgentCatalog {
        models: models.into_values().collect(),
        defaults: CatalogDefaults {
            model: selected,
            mode: None,
        },
        ..Default::default()
    }
}

pub(crate) fn catalog(value: &Value) -> AgentCatalog {
    catalog_for(value, "fx")
}

/// A prompt response reports that turn's token usage, not context occupancy or
/// the provider account's billing period. Preserve native cache/reasoning names.
pub(crate) fn turn_usage(
    value: &Value,
    provider: &str,
    _native_provider: &str,
) -> Option<TokensUsage> {
    let usage = value.get("usage")?;
    let optional = |names: &[&str]| -> Option<Option<u64>> {
        match names.iter().find_map(|name| usage.get(*name)) {
            Some(value) if !value.is_null() => value.as_u64().map(Some),
            _ => Some(None),
        }
    };
    let (read, write, reasoning): (&[&str], &[&str], &[&str]) = if provider == "fx" {
        (
            &["cacheReadTokens"],
            &["cacheWriteTokens"],
            &["reasoningTokens"],
        )
    } else {
        (
            &["cachedReadTokens"],
            &["cachedWriteTokens"],
            &["thoughtTokens"],
        )
    };
    let mut tokens = TokensUsage {
        input: usage["inputTokens"].as_u64()?,
        output: usage["outputTokens"].as_u64()?,
        cache_read: optional(read)?,
        cache_write: optional(write)?,
        reasoning: optional(reasoning)?,
    };
    // FX Codex, AI Gateway and Grok responses include cache/reasoning within
    // their parent counters; custom chat-completions provides no breakdown.
    // The Gateway contract uses disjoint parts, so preserve the actual total.
    if provider == "fx" {
        tokens.input = tokens
            .input
            .checked_sub(tokens.cache_read.unwrap_or(0))?
            .checked_sub(tokens.cache_write.unwrap_or(0))?;
        tokens.output = tokens.output.checked_sub(tokens.reasoning.unwrap_or(0))?;
    }
    Some(tokens)
}

/// Totals are complete only for a new session whose every admitted turn had a
/// valid authoritative reply. Native load supplies no historical token totals.
pub(crate) struct SessionUsage {
    total: Option<TokensUsage>,
    observed: bool,
}
impl SessionUsage {
    pub fn new(new_session: bool) -> Self {
        Self {
            total: new_session.then(TokensUsage::default),
            observed: false,
        }
    }
    pub fn finish(&mut self, usage: Option<TokensUsage>) -> Option<TokensUsage> {
        let merge_optional =
            |a: Option<u64>, b: Option<u64>, parent: &mut u64| -> Option<Option<u64>> {
                match (a, b) {
                    (Some(a), Some(b)) => a.checked_add(b).map(Some),
                    (Some(known), None) | (None, Some(known)) => {
                        *parent = parent.checked_add(known)?;
                        Some(None)
                    }
                    (None, None) => Some(None),
                }
            };
        self.total = self.total.take().zip(usage).and_then(|(total, next)| {
            let mut input = total.input.checked_add(next.input)?;
            let mut output = total.output.checked_add(next.output)?;
            let cache_read = if self.observed {
                merge_optional(total.cache_read, next.cache_read, &mut input)?
            } else {
                next.cache_read
            };
            let cache_write = if self.observed {
                merge_optional(total.cache_write, next.cache_write, &mut input)?
            } else {
                next.cache_write
            };
            let reasoning = if self.observed {
                merge_optional(total.reasoning, next.reasoning, &mut output)?
            } else {
                next.reasoning
            };
            Some(TokensUsage {
                input,
                output,
                cache_read,
                cache_write,
                reasoning,
            })
        });
        self.observed = true;
        self.total.clone()
    }
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
        .or_else(|| {
            tool.pointer("/_meta/qoder/toolName")
                .and_then(Value::as_str)
        })
        .or_else(|| tool["kind"].as_str())
        .unwrap_or("tool");
    // The App's shell safety scanner needs the actual command, never a title
    // or lossy argv rendering. Unknown execute inputs must not reach older
    // Apps as empty-resource requests that YOLO would automatically allow.
    let shell = tool["kind"].as_str() == Some("execute")
        || matches!(name, "shell" | "bash" | "Bash" | "run_shell_command");
    let resources = if shell {
        let input = tool["rawInput"].as_object().ok_or_else(|| {
            AgentError::Protocol("ACP execute permission has no complete command input".into())
        })?;
        if input
            .get("action")
            .is_some_and(|action| action.as_str() != Some("run"))
        {
            return Err(AgentError::Protocol(
                "ACP execute permission has an unsupported shell action".into(),
            ));
        }
        let command = input
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| {
                AgentError::Protocol("ACP execute permission has no complete command input".into())
            })?;
        vec![command.to_string()]
    } else {
        Vec::new()
    };
    let request = PermissionRequest {
        id,
        asid: session.into(),
        action: if shell { "shell" } else { name }.into(),
        resources,
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
    permission_contexts: HashMap<String, Value>,
    permission_turn_active: bool,
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
            permission_contexts: HashMap::new(),
            permission_turn_active: false,
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

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn latest_text(&self, role: TimelineRole) -> Option<&str> {
        self.rows
            .values()
            .filter(|row| row.role == role)
            .filter_map(|row| match &row.part {
                AgentPart::Text { text } => Some((row.ordinal, text.as_str())),
                _ => None,
            })
            .max_by_key(|(ordinal, _)| *ordinal)
            .map(|(_, text)| text)
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
        self.permission_contexts.clear();
        self.permission_turn_active = true;
        self.current = None;
        self.thought = None;
        self.anonymous = None;
    }

    /// Recover omitted input only from this submitted turn's unfinished call,
    /// never native history or a previous turn that reused an opaque tool id.
    pub fn permission(&self, rpc_id: Value, params: &Value) -> Result<Permission, AgentError> {
        let mut complete = params.clone();
        if let Some(context) = params["toolCall"]["toolCallId"]
            .as_str()
            .and_then(|id| self.permission_contexts.get(id))
        {
            if let Some(tool) = complete.get_mut("toolCall").and_then(Value::as_object_mut) {
                for field in ["name", "kind", "rawInput", "_meta"] {
                    if !tool.contains_key(field) {
                        if let Some(value) = context.get(field) {
                            tool.insert(field.into(), value.clone());
                        }
                    }
                }
            }
        }
        permission(rpc_id, &complete)
    }

    pub fn end_turn(&mut self) {
        self.permission_contexts.clear();
        self.permission_turn_active = false;
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
            "notice" => {
                let severity = match update["severity"].as_str() {
                    Some("info") => "Agent notice",
                    Some("warning") => "Agent warning",
                    Some("error") => "Agent error",
                    _ => "Agent notice (unknown severity)",
                };
                let title = update["title"]
                    .as_str()
                    .unwrap_or("The agent sent a notice without a readable title.");
                // Native notice text is untrusted display data. Bound this row
                // independently, retain visible severity, and make truncation explicit.
                const MAX_NOTICE_BYTES: usize = 64 * 1024;
                let mut text = String::with_capacity(title.len().min(MAX_NOTICE_BYTES));
                for ch in title
                    .chars()
                    .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                {
                    if text.len() + ch.len_utf8() > MAX_NOTICE_BYTES {
                        text.push_str("\n[Notice truncated by Gateway; open the agent's terminal for the complete diagnostic.]");
                        break;
                    }
                    text.push(ch);
                }
                if text.is_empty() {
                    text.push_str("The agent sent an empty notice.");
                }
                let id = format!("acp-notice:{}", self.next_row + 1);
                let group = self.group(&id);
                Ok(Some(self.row(
                    id,
                    group,
                    TimelineRole::System,
                    AgentPart::System {
                        text: format!("{severity}: {text}"),
                        description: Some("ACP session notice".into()),
                    },
                )))
            }
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
                if self.permission_turn_active {
                    if matches!(
                        call.state,
                        ToolCallStatus::Pending | ToolCallStatus::Running
                    ) {
                        let context = self
                            .permission_contexts
                            .entry(native.into())
                            .or_insert_with(|| Value::Object(Default::default()));
                        for field in ["name", "kind", "rawInput", "_meta"] {
                            if let Some(value) = update.get(field) {
                                context[field] = value.clone();
                            }
                        }
                    } else {
                        self.permission_contexts.remove(native);
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
    fn notices_remain_separate_from_literal_assistant_chunks() {
        let mut projection = Projection::default();
        let literal =
            "[context] skill catalog shortened 97 descriptions\n\u{1b}literal assistant text";
        projection.update(&json!({"sessionUpdate":"agent_message_chunk","messageId":"answer","content":{"type":"text","text":literal}})).unwrap();
        for severity in ["info", "warning", "error", "future-severity"] {
            let row=projection.update(&json!({"sessionUpdate":"notice","severity":severity,"title":"Diagnostic\u{0}\u{7}\r\nnext line"})).unwrap().unwrap();
            assert_eq!(row.role, TimelineRole::System);
            assert!(
                matches!(&row.part,AgentPart::System{text,..} if text.ends_with("Diagnostic\nnext line") && !text.contains('\0') && !text.contains('\u{7}'))
            );
            if severity == "future-severity" {
                assert!(
                    matches!(&row.part,AgentPart::System{text,..} if text.contains("unknown severity"))
                );
            }
        }
        let row=projection.update(&json!({"sessionUpdate":"agent_message_chunk","messageId":"answer","content":{"type":"text","text":"\nEnd"}})).unwrap().unwrap();
        assert_eq!(row.role, TimelineRole::Assistant);
        assert!(matches!(&row.part,AgentPart::Text{text} if text == &format!("{literal}\nEnd")));
        assert_eq!(projection.timeline().len(), 5);
    }

    #[test]
    fn oversized_and_malformed_notices_keep_visible_severity_and_limits() {
        let mut projection = Projection::default();
        let row = projection
            .update(
                &json!({"sessionUpdate":"notice","severity":"error","title":"界".repeat(30_000)}),
            )
            .unwrap()
            .unwrap();
        let AgentPart::System { text, .. } = row.part else {
            panic!("notice must remain a system row")
        };
        assert!(text.starts_with("Agent error:"));
        assert!(text.contains("Notice truncated by Gateway"));
        assert!(text.len() < 66_000);
        let row = projection
            .update(&json!({"sessionUpdate":"notice","severity":"error"}))
            .unwrap()
            .unwrap();
        assert!(
            matches!(&row.part,AgentPart::System{text,..} if text.starts_with("Agent error:") && text.contains("without a readable title"))
        );
    }

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

    fn shell_permission(input: Option<Value>) -> Value {
        let mut params = json!({"sessionId":"s","toolCall":{"toolCallId":"tool","kind":"execute"},"options":[{"optionId":"once","kind":"allow_once"},{"optionId":"no","kind":"reject_once"}]});
        if let Some(input) = input {
            params["toolCall"]["rawInput"] = input;
        }
        params
    }

    #[test]
    fn native_execute_permissions_preserve_the_complete_shell_command() {
        let command = "sudo rm -rf /\nprintf 'full input must remain visible'";
        for input in [
            json!({"action":"run","command":command}),
            json!({"command":command}),
        ] {
            let mut params = shell_permission(Some(input));
            params["toolCall"]["_meta"] = json!({"qoder":{"toolName":"Bash"}});
            let parsed = permission(json!(1), &params).unwrap();
            assert_eq!(parsed.request.action, "shell");
            assert_eq!(parsed.request.resources, [command]);
        }
    }

    #[test]
    fn unknown_execute_inputs_cannot_become_auto_approvable_requests() {
        for input in [
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!({"command":" "})),
            Some(json!({"command":["rm","-rf","/"]})),
            Some(json!({"code":"rm -rf /"})),
            Some(json!({"action":"write","command":"printf misleading"})),
        ] {
            assert!(permission(json!(1), &shell_permission(input)).is_err());
        }
        let ordinary = json!({"sessionId":"s","toolCall":{"toolCallId":"question","kind":"other","title":"Choose an option"},"options":[{"optionId":"yes","kind":"allow_once"}]});
        assert!(permission(json!(2), &ordinary).is_ok());
    }

    #[test]
    fn permission_patch_recovers_only_this_turns_unfinished_tool_input() {
        let mut projection = Projection::new(false);
        let update = json!({"sessionUpdate":"tool_call","toolCallId":"tool","name":"shell","kind":"execute","status":"pending","rawInput":{"action":"run","command":"rm -rf /"}});
        // History is not a live submitted turn.
        projection.update(&update).unwrap();
        assert!(projection
            .permission(json!(1), &shell_permission(None))
            .is_err());
        projection.submitted("start");
        projection.update(&update).unwrap();
        assert_eq!(
            projection
                .permission(json!(1), &shell_permission(None))
                .unwrap()
                .request
                .resources,
            ["rm -rf /"]
        );
        // Explicitly malformed input never falls back to earlier input.
        assert!(projection
            .permission(json!(1), &shell_permission(Some(Value::Null)))
            .is_err());
        let mut other = shell_permission(None);
        other["toolCall"]["toolCallId"] = json!("different");
        assert!(projection.permission(json!(1), &other).is_err());
        projection.update(&json!({"sessionUpdate":"tool_call_update","toolCallId":"tool","status":"completed"})).unwrap();
        assert!(projection
            .permission(json!(1), &shell_permission(None))
            .is_err());

        let mut projection = Projection::new(false);
        projection.submitted("one");
        projection.update(&update).unwrap();
        projection.end_turn();
        assert!(projection
            .permission(json!(1), &shell_permission(None))
            .is_err());
        projection.submitted("two");
        assert!(projection
            .permission(json!(1), &shell_permission(None))
            .is_err());
    }

    #[test]
    fn cold_model_names_resolve_only_to_one_native_option() {
        let options = json!({"configOptions":[{"id":"model","options":[{"value":"qfmodel","name":"Qwen3.8-Flash"},{"value":"dfmodel","name":"DeepSeek-Flash"}]}]});
        assert_eq!(
            resolve_model_value(&options, "Qwen3.8-Flash").unwrap(),
            "qfmodel"
        );
        assert_eq!(resolve_model_value(&options, "dfmodel").unwrap(), "dfmodel");
        assert!(resolve_model_value(&options, "invented").is_err());
        assert!(resolve_model_value(&json!({"configOptions":[{"id":"model","options":[{"value":"one","name":"Same"},{"value":"two","name":"Same"}]}]}),"Same").is_err());
    }

    #[test]
    fn incomplete_optional_breakdown_keeps_the_exact_known_total() {
        let mut session = SessionUsage::new(true);
        let complete=turn_usage(&json!({"usage":{"inputTokens":17,"outputTokens":7,"cacheReadTokens":5,"cacheWriteTokens":2,"reasoningTokens":3}}),"fx","codex").unwrap();
        session.finish(Some(complete)).unwrap();
        let next = turn_usage(
            &json!({"usage":{"inputTokens":17,"outputTokens":7}}),
            "fx",
            "codex",
        )
        .unwrap();
        let total = session.finish(Some(next)).unwrap();
        assert_eq!(total.input + total.output, 48);
        assert!(
            total.cache_read.is_none() && total.cache_write.is_none() && total.reasoning.is_none()
        );
    }

    #[test]
    fn native_model_catalog_and_grouped_effort_keep_opaque_choices() {
        let value = json!({"models":{"currentModelId":"m-one","availableModels":[{"modelId":"m-one","name":"Native one"},{"modelId":"m-two","name":"Native two"}]},"configOptions":[{"id":"reasoning_effort","currentValue":"high","options":[{"group":"Thinking","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}]}]});
        let catalog = catalog_for(&value, "qoder");
        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog.defaults.model.as_ref().unwrap().provider_id,
            "qoder"
        );
        assert_eq!(
            catalog.defaults.model.as_ref().unwrap().variant.as_deref(),
            Some("high")
        );
        assert_eq!(catalog.models[0].variants.as_ref().unwrap().len(), 2);
        assert!(catalog.models[1].variants.is_none());
        assert!(offers_config_value(&value, "reasoning_effort", "high"));
        assert!(!offers_config_value(&value, "reasoning_effort", "invented"));
    }

    #[test]
    fn prompt_usage_preserves_disjoint_totals_and_unknown_optional_counters() {
        let value = json!({"usage":{"inputTokens":17,"outputTokens":7,"cacheReadTokens":5,"cacheWriteTokens":2,"reasoningTokens":3}});
        let usage = turn_usage(&value, "fx", "codex").unwrap();
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.cache_write,
                usage.reasoning
            ),
            (10, 4, Some(5), Some(2), Some(3))
        );
        let qoder = turn_usage(
            &json!({"usage":{"inputTokens":9,"outputTokens":4,"totalTokens":13}}),
            "qoder",
            "qoder",
        )
        .unwrap();
        assert_eq!((qoder.input, qoder.output), (9, 4));
        assert!(qoder.reasoning.is_none());
        assert!(turn_usage(
            &json!({"usage":{"inputTokens":3,"outputTokens":4,"cacheReadTokens":5}}),
            "fx",
            "codex"
        )
        .is_none());
        assert!(turn_usage(&json!({"usage":{"inputTokens":3}}), "fx", "codex").is_none());
    }

    #[test]
    fn session_totals_require_complete_history_and_every_authoritative_turn() {
        let turn = TokensUsage {
            input: 10,
            output: 4,
            ..Default::default()
        };
        let mut fresh = SessionUsage::new(true);
        assert_eq!(fresh.finish(Some(turn.clone())).unwrap().input, 10);
        assert_eq!(fresh.finish(Some(turn.clone())).unwrap().input, 20);
        assert!(fresh.finish(None).is_none());
        assert!(fresh.finish(Some(turn.clone())).is_none());
        let mut restored = SessionUsage::new(false);
        assert!(restored.finish(Some(turn)).is_none());
        let mut overflow = SessionUsage::new(true);
        overflow
            .finish(Some(TokensUsage {
                input: u64::MAX,
                ..Default::default()
            }))
            .unwrap();
        assert!(overflow
            .finish(Some(TokensUsage {
                input: 1,
                ..Default::default()
            }))
            .is_none());
    }

    #[test]
    fn providerless_model_metadata_keeps_native_selection_and_effort_namespace() {
        let value = json!({"configOptions":[{"id":"model","currentValue":"native-model"},{"id":"reasoning_effort","currentValue":"xhigh"}]});
        let model = selected_model_for(&value, "qoder").unwrap();
        assert_eq!(model.provider_id, "qoder");
        assert_eq!(model.model_id, "native-model");
        assert_eq!(model.variant.as_deref(), Some("xhigh"));
        let catalog = catalog_for(&value, "qoder");
        assert!(catalog.models.is_empty());
        assert!(catalog.modes.is_empty());
        assert_eq!(catalog.defaults.model, Some(model));
        assert!(catalog.defaults.mode.is_none());
    }
}
