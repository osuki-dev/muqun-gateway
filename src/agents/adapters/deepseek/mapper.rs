use serde_json::{json, Value};

use crate::agents::domain::{
    reasoning_item_id, text_item_id, tool_item_id, AgentCatalog, AgentErrorInfo, AgentPart,
    AgentSessionId, AgentSessionInfo, AgentSessionStatus, CatalogDefaults, CommandInfo,
    CompactionStatus, FormField, FormOption, FormRequest, ModeInfo, ModelInfo, ModelRef,
    ModelVariantInfo, PermissionDecision, PermissionOption, PermissionRequest, ProviderInfo,
    ProviderModelInfo, SkillInfo, TimelineItem, TimelineRole, TokensUsage, ToolCall,
    ToolCallStatus, ToolTime,
};

/// Map a DeepSeek Harness `SessionSummary` JSON object into Gateway `AgentSessionInfo`.
pub fn map_session(raw: &Value) -> Option<AgentSessionInfo> {
    let session_id = raw.get("sessionId").and_then(Value::as_str)?;
    let running = raw.get("running").and_then(Value::as_bool).unwrap_or(false);
    let updated_at = raw
        .get("updatedAt")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });

    let projections_values = raw.get("projections").and_then(|p| p.get("values"));

    let cwd = raw
        .get("cwd")
        .or_else(|| projections_values.and_then(|v| v.get("cwd")))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let parent_id = raw
        .get("parentSessionId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Read model from projection hints if present
    let model = projections_values
        .and_then(|v| v.get("modelSelection"))
        .and_then(|ms| ms.get("next").or_else(|| ms.get("lastUsed")))
        .and_then(|sel| {
            let provider = sel.get("provider").and_then(Value::as_str)?;
            let model = sel.get("model").and_then(Value::as_str)?;
            let variant = sel
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(ModelRef {
                provider_id: provider.to_string(),
                model_id: model.to_string(),
                variant,
            })
        });

    // Derive tokens usage
    let tokens = projections_values
        .and_then(|v| v.get("tokenUsage"))
        .map(|tu| {
            let uncached = tu
                .get("uncachedInputTokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = tu.get("outputTokens").and_then(Value::as_u64).unwrap_or(0);
            let cache_read = tu.get("cacheReadTokens").and_then(Value::as_u64);
            let cache_write = tu.get("cacheWriteTokens").and_then(Value::as_u64);
            TokensUsage {
                input: uncached + cache_read.unwrap_or(0),
                output,
                reasoning: None,
                cache_read,
                cache_write,
            }
        });

    // Derive a clean title
    let title = raw
        .get("title")
        .or_else(|| projections_values.and_then(|v| v.get("title")))
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or("DeepSeek Agent Session")
        .to_string();

    let status = if running {
        AgentSessionStatus::Busy
    } else {
        AgentSessionStatus::Idle
    };

    Some(AgentSessionInfo {
        asid: AgentSessionId(session_id.to_string()),
        agent_id: String::new(),
        backend_session_id: session_id.to_string(),
        title,
        mode: projections_values
            .and_then(|v| v.get("agentPreset"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        model,
        status,
        directory: cwd,
        cost: None,
        tokens,
        limit: None,
        parent_id,
        project_id: None,
        outcome: None,
        error: None,
        revert: None,
        fork: None,
        time_idle: None,
        time_viewed: None,
        deleted: false,
        updated_ms: updated_at,
    })
}

/// Map a DeepSeek Harness `ModelCatalog` JSON response into Gateway `AgentCatalog`.
///
/// `presets` is the `agentPresets/list` roster (`{ "presets": [{ id, isDefault,
/// name?, description?, broken? }] }`), the only source of modes: DSH's
/// `session/create` accepts exactly those ids as `agentPreset`. Without a
/// roster the catalog advertises no modes rather than inventing one.
pub fn map_catalog(raw: &Value, presets: Option<&Value>) -> AgentCatalog {
    let mut models = Vec::new();
    let mut providers = Vec::new();

    // Default model if available
    let default_model_ref = raw.get("default").and_then(|d| {
        let provider = d.get("provider").and_then(Value::as_str)?;
        let model = d.get("model").and_then(Value::as_str)?;
        let variant = d
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(ModelRef {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
            variant,
        })
    });

    if let Some(groups) = raw.get("groups").and_then(Value::as_array) {
        for group in groups {
            let provider_id = group
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("deepseek");
            let provider_name = group
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(provider_id);

            let mut group_models = Vec::new();

            if let Some(model_list) = group.get("models").and_then(Value::as_array) {
                for m in model_list {
                    let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = m.get("name").and_then(Value::as_str).unwrap_or(id);
                    if id.is_empty() {
                        continue;
                    }

                    let mut variants = Vec::new();
                    if let Some(efforts) = m
                        .get("reasoning")
                        .and_then(|r| r.get("efforts"))
                        .and_then(Value::as_array)
                    {
                        for effort in efforts {
                            if let Some(eid) = effort.get("id").and_then(Value::as_str) {
                                variants.push(ModelVariantInfo {
                                    id: eid.to_string(),
                                    reasoning_effort: Some(eid.to_string()),
                                });
                            }
                        }
                    }
                    if variants.is_empty() {
                        variants.push(ModelVariantInfo {
                            id: "default".to_string(),
                            reasoning_effort: None,
                        });
                    }

                    models.push(ModelInfo {
                        id: id.to_string(),
                        name: name.to_string(),
                        provider_id: provider_id.to_string(),
                        family: None,
                        limit: None,
                        variants: Some(variants.clone()),
                        cost: None,
                        enabled: true,
                        status: Some("active".to_string()),
                    });
                    group_models.push(ProviderModelInfo {
                        id: id.to_string(),
                        name: name.to_string(),
                        enabled: true,
                        variants,
                        limit: None,
                        status: Some("active".to_string()),
                    });
                }
            }

            providers.push(ProviderInfo {
                id: provider_id.to_string(),
                name: provider_name.to_string(),
                activation: Some("enabled".to_string()),
                models: group_models,
            });
        }
    }

    let mut modes = Vec::new();
    let mut default_mode = None;
    let rows = presets
        .and_then(|p| p.get("presets"))
        .and_then(Value::as_array);
    for row in rows.into_iter().flatten() {
        let Some(id) = row
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // A preset that failed to activate cannot start a session.
        if row.get("broken").is_some() {
            continue;
        }
        if default_mode.is_none() && row.get("isDefault").and_then(Value::as_bool) == Some(true) {
            default_mode = Some(id.to_string());
        }
        modes.push(ModeInfo {
            id: id.to_string(),
            name: row
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string(),
            description: row
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            mode: Some("primary".to_string()),
            color: None,
            hidden: false,
            model: None,
        });
    }

    AgentCatalog {
        models,
        modes,
        mcp: vec![],
        skills: vec![SkillInfo {
            id: "deepseek".to_string(),
            name: "deepseek".to_string(),
            description: "DeepSeek tools".to_string(),
            slash: true,
            autoinvoke: true,
        }],
        providers,
        commands: vec![CommandInfo {
            name: "help".to_string(),
            description: Some("Show available help".to_string()),
            mode: None,
            template: None,
        }],
        defaults: CatalogDefaults {
            model: default_model_ref,
            mode: default_mode,
        },
    }
}

// ---------------------------------------------------------------------------
// Timeline rows
//
// DeepSeek Harness's message ids are random UUIDs (`freezeMessage` in
// dsh-llm/lib/index.js), and the mirror orders rows by message id, so a row's
// message id is the zero-padded log seq of the event that produced it. That
// is chronological by construction and the same whether the row came from a
// `session/page` read or a live `session/follow` event, which is what keeps
// the two from producing two rows for one message.
// ---------------------------------------------------------------------------

/// The message id of the message a session event at `seq` produced.
pub fn record_message_id(seq: u64) -> String {
    format!("s{seq:012}")
}

/// The message id an assistant attempt streams under before it commits.
/// It sorts right after the last durable seq the attempt saw and before the
/// next one, so the rows appear where the committed message will land, and
/// is replaced by [`record_message_id`] rows when the `assistant/message`
/// event arrives.
pub fn provisional_message_id(started_after_seq: u64, attempt_id: &str) -> String {
    format!("{}~{attempt_id}", record_message_id(started_after_seq))
}

/// A tool call's `arguments` is the raw JSON string the model produced.
/// Parsed when it is valid; kept verbatim under `raw` when it is not (a call
/// cut off mid-stream), so nothing the model said is lost.
pub fn parse_tool_arguments(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    serde_json::from_str::<Value>(trimmed).unwrap_or_else(|_| json!({ "raw": raw }))
}

/// Join the text of a `ContentBlock[]`.
pub fn content_text(blocks: &Value) -> String {
    let Some(blocks) = blocks.as_array() else {
        return blocks.as_str().unwrap_or("").to_string();
    };
    let mut out = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// What one `tool/result` event says about a call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Value,
    pub output: Option<Value>,
    pub is_error: bool,
    pub error: Option<AgentErrorInfo>,
}

/// Read a `tool/result` event's `data`.
pub fn map_tool_result(data: &Value) -> Option<ToolResult> {
    let message = data.get("message")?;
    let call_id = message
        .get("toolCallId")
        .or_else(|| data.get("callId"))
        .and_then(Value::as_str)?
        .to_string();
    let content = message.get("content").cloned().unwrap_or(json!([]));
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = content_text(&content);
    let output = if text.is_empty() {
        None
    } else {
        Some(Value::String(text))
    };
    let error = data.get("error").map(|e| AgentErrorInfo {
        name: e
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("ToolError")
            .to_string(),
        message: e
            .get("reason")
            .or_else(|| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("tool failed")
            .to_string(),
        status: None,
    });
    Some(ToolResult {
        call_id,
        content,
        output,
        is_error,
        error,
    })
}

/// Apply a result to its call.
pub fn apply_tool_result(call: &mut ToolCall, result: &ToolResult, completed_ms: u64) {
    call.output = result.output.clone();
    call.content = Some(result.content.clone());
    call.set_state(if result.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    });
    if result.error.is_some() {
        call.error = result.error.clone();
    }
    call.time.completed = Some(completed_ms);
}

fn message_role(role: &str) -> TimelineRole {
    match role {
        "assistant" => TimelineRole::Assistant,
        "system" | "developer" => TimelineRole::System,
        _ => TimelineRole::User,
    }
}

/// The producer kind of a `user/message` the harness injected rather than the
/// user typed. dsh stamps every message with `source.kind`
/// (`MessageSourceMap`, dsh-llm `types/message.d.ts`): a typed prompt is
/// `"user"` (`user-rpc` in dsh-api-session-controller), while producers use
/// their own kind, e.g. `runtime-context` (dsh-agent-loop), `time-context`,
/// `skill-catalog` (dsh-tool-skill), `skill-invocation` (dsh-skill). A record
/// with no `source` is kept as a user message.
fn injected_source_kind(data: &Value) -> Option<&str> {
    data.pointer("/source/kind")
        .and_then(Value::as_str)
        .filter(|kind| *kind != "user")
}

/// Re-express injected text as a `system` row carrying a `Synthetic` part,
/// the way the OpenCode mapper treats `Session.Message.Synthetic`, so the App
/// does not render it as a user bubble.
fn mark_injected(items: &mut [TimelineItem], kind: &str) {
    for item in items {
        if let AgentPart::Text { text } = &mut item.part {
            item.part = AgentPart::Synthetic {
                text: std::mem::take(text),
                description: Some(kind.to_string()),
            };
            item.role = TimelineRole::System;
        }
    }
}

/// Rows for one message's `ContentBlock[]`, under `message_id`.
fn map_content_blocks(
    message_id: &str,
    role: TimelineRole,
    blocks: &Value,
    seq: u64,
    time: u64,
) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let Some(blocks) = blocks.as_array() else {
        return items;
    };
    for (idx, block) in blocks.iter().enumerate() {
        let ordinal = idx as u64;
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("text");
        let (id, part) = match kind {
            "text" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                (
                    text_item_id(message_id, ordinal),
                    AgentPart::Text {
                        text: text.to_string(),
                    },
                )
            }
            "reasoning" => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                (
                    reasoning_item_id(message_id, ordinal),
                    AgentPart::Reasoning {
                        text: text.to_string(),
                        duration_ms: None,
                    },
                )
            }
            "tool-call" | "tool_call" | "tool" => {
                let call_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let input = match block.get("arguments") {
                    Some(Value::String(raw)) => parse_tool_arguments(raw),
                    Some(other) => other.clone(),
                    None => block.get("input").cloned().unwrap_or(json!({})),
                };
                let mut call = project_tool_call(call_id.clone(), name, input, None, false);
                // Nothing has answered it yet; a `tool/result` will.
                call.set_state(ToolCallStatus::Running);
                call.time.created = Some(time);
                (tool_item_id(message_id, &call_id), AgentPart::Tool(call))
            }
            // Images, tool additions and unknown block kinds have no row.
            _ => continue,
        };
        items.push(TimelineItem {
            id,
            message_id: message_id.to_string(),
            role,
            part,
            seq,
            updated_ms: time,
            ordinal,
            attachments: None,
        });
    }
    items
}

/// Map one `SessionWireEvent` (`{type, seq, time, data}`) to timeline rows.
/// Only message-producing events yield rows; `tool/result` is joined onto
/// its call by the caller because the call may live in an earlier event.
pub fn map_record(event: &Value) -> Vec<TimelineItem> {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
    let time = event.get("time").and_then(Value::as_u64).unwrap_or(0);
    let Some(data) = event.get("data") else {
        return Vec::new();
    };
    let message_id = record_message_id(seq);
    match event_type {
        "user/message" => {
            let role = message_role(data.get("role").and_then(Value::as_str).unwrap_or("user"));
            let blocks = data.get("content").cloned().unwrap_or(Value::Null);
            let mut items = map_content_blocks(&message_id, role, &blocks, seq, time);
            if let Some(kind) = injected_source_kind(data) {
                mark_injected(&mut items, kind);
            }
            items
        }
        "assistant/message" => {
            let message = data.get("message").unwrap_or(data);
            let blocks = message.get("content").cloned().unwrap_or(Value::Null);
            map_content_blocks(&message_id, TimelineRole::Assistant, &blocks, seq, time)
        }
        "compaction/summary" => {
            let summary = content_text(data.get("summary").unwrap_or(&Value::Null));
            vec![TimelineItem {
                id: format!("{message_id}:compaction"),
                message_id,
                role: TimelineRole::System,
                part: AgentPart::Compaction {
                    status: CompactionStatus::Completed,
                    reason: None,
                    summary: if summary.is_empty() {
                        None
                    } else {
                        Some(summary)
                    },
                    recent: None,
                    tokens: data.get("usage").and_then(map_usage),
                    cost: None,
                    error: None,
                },
                seq,
                updated_ms: time,
                ordinal: 0,
                attachments: None,
            }]
        }
        _ => Vec::new(),
    }
}

/// Map the records of a `session/page` (or a follow snapshot) into rows,
/// with every `tool/result` in the page joined onto its call.
pub fn map_timeline_records(raw_page: &Value) -> Vec<TimelineItem> {
    let mut items: Vec<TimelineItem> = Vec::new();
    let Some(records) = raw_page.get("records").and_then(Value::as_array) else {
        return items;
    };
    for entry in records {
        let Some(event) = entry.get("event") else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "tool/result" {
            let time = event.get("time").and_then(Value::as_u64).unwrap_or(0);
            let Some(result) = event.get("data").and_then(map_tool_result) else {
                continue;
            };
            for item in items.iter_mut() {
                if let AgentPart::Tool(ref mut call) = item.part {
                    if call.id == result.call_id {
                        apply_tool_result(call, &result, time);
                        item.updated_ms = time;
                    }
                }
            }
            continue;
        }
        items.extend(map_record(event));
    }
    items
}

/// Project tool call metadata so App UI components (like code diff or terminal)
/// receive the exact camelCase / snake_case fields they expect.
pub fn project_tool_call(
    id: String,
    name: String,
    input: Value,
    output: Option<Value>,
    is_error: bool,
) -> ToolCall {
    let mut metadata = json!({});

    // 1. Files / Diff projection for edit/patch tools
    if name == "edit" || name == "write" || name == "fs_edit" || name == "str_replace_editor" {
        let file_path = input
            .get("path")
            .or_else(|| input.get("file"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let patch_text = output
            .as_ref()
            .and_then(|o| {
                o.get("patch")
                    .or_else(|| o.get("diff"))
                    .or_else(|| o.get("text"))
            })
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        metadata["files"] = json!([
            {
                "path": file_path,
                "patch": patch_text,
                "additions": 0,
                "deletions": 0
            }
        ]);
    }

    // 2. Shell / Command execution projection
    if name == "shell" || name == "bash" || name == "terminal" || name == "execute" {
        let exit_code = if is_error { 1 } else { 0 };
        metadata["exitCode"] = json!(exit_code);
        metadata["exit"] = json!(exit_code);
        metadata["shellId"] = json!(format!("shell_{id}"));
    }

    // 3. Subagent projection
    if name == "subagent" || name == "task" {
        metadata["sessionID"] = json!(format!("child_{id}"));
        metadata["status"] = json!(if is_error { "failed" } else { "completed" });
    }

    let status = if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };

    ToolCall {
        id: id.clone(),
        name: name.clone(),
        title: Some(format!("{name}: {id}")),
        input,
        output: output.clone(),
        content: output.map(|o| json!([{"type": "text", "text": o.to_string()}])),
        metadata: Some(metadata),
        state: status,
        status,
        error: None,
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated: false,
        time: ToolTime::default(),
    }
}

// ---------------------------------------------------------------------------
// Session facts carried by the live streams
// ---------------------------------------------------------------------------

/// `TokenUsage` from dsh-llm: `inputTokens` is uncached input only.
pub fn map_usage(usage: &Value) -> Option<TokensUsage> {
    let input = usage.get("inputTokens").and_then(Value::as_u64);
    let output = usage.get("outputTokens").and_then(Value::as_u64);
    if input.is_none() && output.is_none() {
        return None;
    }
    let cache_read = usage.get("cacheReadTokens").and_then(Value::as_u64);
    Some(TokensUsage {
        input: input.unwrap_or(0) + cache_read.unwrap_or(0),
        output: output.unwrap_or(0),
        reasoning: usage.get("reasoningTokens").and_then(Value::as_u64),
        cache_read,
        cache_write: usage.get("cacheWriteTokens").and_then(Value::as_u64),
    })
}

/// `turn/end` -> the session status it leaves behind. `TurnEndReasonMap`:
/// `completed`, `aborted`, `blocked`, `error`, `max-tokens`.
pub fn map_turn_end(data: &Value) -> (AgentSessionStatus, Option<AgentErrorInfo>) {
    match data.pointer("/reason/kind").and_then(Value::as_str) {
        Some("aborted") => (AgentSessionStatus::Interrupted, None),
        Some("error") => {
            let failure = data.pointer("/reason/error");
            let error = AgentErrorInfo {
                name: failure
                    .and_then(|f| f.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("TurnError")
                    .to_string(),
                message: failure
                    .and_then(|f| f.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("the turn failed")
                    .to_string(),
                status: None,
            };
            (AgentSessionStatus::Failed, Some(error))
        }
        _ => (AgentSessionStatus::Idle, None),
    }
}

/// `api-session/error` carries only a message.
pub fn map_error_message(message: &str) -> AgentErrorInfo {
    AgentErrorInfo {
        name: "AgentError".to_string(),
        message: message.to_string(),
        status: None,
    }
}

/// A follow's opening `snapshot`: the wire header and projection baseline,
/// read through [`map_session`] so the snapshot and `session/list` agree.
pub fn map_snapshot_session(
    session_id: &str,
    header: &Value,
    projections: &Value,
) -> Option<AgentSessionInfo> {
    let mut summary = json!({
        "sessionId": session_id,
        "running": false,
        "projections": { "values": projections.get("values").cloned().unwrap_or(json!({})) },
    });
    if let Some(cwd) = header.get("cwd") {
        summary["cwd"] = cwd.clone();
    }
    if let Some(parent) = header.get("parentSession") {
        summary["parentSessionId"] = parent.clone();
    }
    let mut info = map_session(&summary)?;
    if let Some(preset) = header.get("agentPreset").and_then(Value::as_str) {
        info.mode = Some(preset.to_string());
    }
    Some(info)
}

// ---------------------------------------------------------------------------
// Interactive requests forwarded on `$events`
// ---------------------------------------------------------------------------

/// The `approval/request` waterfall payload -> a permission prompt. The
/// request is `ApprovalRequestEvent` less `agent`/`signal`: `{toolName,
/// callId?, reason?, displayReason?}`. The outcome vocabulary is
/// `allowed-once | rejected`, so there is no "always" to offer.
pub fn map_approval_request(event_id: &str, agent_id: &str, request: &Value) -> PermissionRequest {
    let tool = request
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let reason = request
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let display = request
        .pointer("/displayReason/en")
        .and_then(Value::as_str)
        .map(str::to_string);
    let prompt = display
        .clone()
        .or_else(|| reason.clone())
        .unwrap_or_else(|| format!("Allow {tool}?"));
    PermissionRequest {
        id: event_id.to_string(),
        asid: AgentSessionId(agent_id.to_string()),
        action: tool.clone(),
        resources: Vec::new(),
        save: Vec::new(),
        prompt,
        tool: Some(tool),
        source_message_id: None,
        source_tool_call_id: request
            .get("callId")
            .and_then(Value::as_str)
            .map(str::to_string),
        metadata: None,
        message: reason,
        options: vec![
            PermissionOption {
                index: 0,
                label: "Allow Once".to_string(),
                decision: PermissionDecision::Allow,
            },
            PermissionOption {
                index: 1,
                label: "Reject".to_string(),
                decision: PermissionDecision::Deny,
            },
        ],
    }
}

/// The `ApprovalOutcome` a decision becomes. DeepSeek Harness grants one shot or
/// rejects; an "always" from the app is honoured as the one-shot grant it
/// can express.
pub fn approval_outcome(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow | PermissionDecision::AllowAlways => "allowed-once",
        PermissionDecision::Deny => "rejected",
    }
}

fn question_options(question: &Value) -> Vec<FormOption> {
    question
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|o| {
                    let label = o.get("label").and_then(Value::as_str)?;
                    Some(FormOption {
                        value: label.to_string(),
                        label: label.to_string(),
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `user-questions/request` waterfall payload -> a form. Each
/// `AskUserQuestionItem` becomes one field keyed by the question id; the
/// option *label* is the value, because that is what the answer echoes.
pub fn map_question_request(
    event_id: &str,
    agent_id: &str,
    request: &Value,
) -> Option<FormRequest> {
    let questions = request.get("questions").and_then(Value::as_array)?;
    let mut fields = Vec::new();
    for question in questions {
        let Some(key) = question.get("id").and_then(Value::as_str) else {
            continue;
        };
        let title = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or(key)
            .to_string();
        let description = question
            .get("detail")
            .and_then(Value::as_str)
            .map(str::to_string);
        let options = question_options(question);
        let multi = question
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if multi && !options.is_empty() {
            fields.push(FormField::Multiselect {
                key: key.to_string(),
                title,
                description,
                required: true,
                when: Vec::new(),
                options,
                default: Vec::new(),
            });
        } else {
            fields.push(FormField::String {
                key: key.to_string(),
                title,
                description,
                required: true,
                when: Vec::new(),
                placeholder: None,
                default: None,
                custom: !options.is_empty(),
                options,
                format: None,
                min_length: None,
                max_length: None,
                pattern: None,
            });
        }
    }
    if fields.is_empty() {
        return None;
    }
    let title = questions
        .iter()
        .find_map(|q| q.get("header").and_then(Value::as_str))
        .unwrap_or("Question")
        .to_string();
    Some(FormRequest {
        id: event_id.to_string(),
        asid: AgentSessionId(agent_id.to_string()),
        title,
        fields,
    })
}

/// Rebuild an `AskUserQuestionAnswer` from the app's `{key: value}` answers.
/// A value naming an option is a selection; anything else is the free-text
/// `custom` answer.
pub fn question_answers(questions: &Value, answers: &Value) -> Value {
    let mut out = Vec::new();
    for question in questions
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = question.get("id").and_then(Value::as_str) else {
            continue;
        };
        let labels: Vec<String> = question_options(question)
            .into_iter()
            .map(|o| o.label)
            .collect();
        let mut selected: Vec<String> = Vec::new();
        let mut custom: Option<String> = None;
        match answers.get(id) {
            Some(Value::Array(values)) => {
                for value in values.iter().filter_map(Value::as_str) {
                    if labels.iter().any(|l| l == value) {
                        selected.push(value.to_string());
                    } else if !value.trim().is_empty() {
                        custom = Some(value.to_string());
                    }
                }
            }
            Some(Value::String(value)) => {
                if labels.iter().any(|l| l == value) {
                    selected.push(value.clone());
                } else if !value.trim().is_empty() {
                    custom = Some(value.clone());
                }
            }
            Some(Value::Null) | None => {}
            Some(other) => custom = Some(other.to_string()),
        }
        let mut answer = json!({ "id": id, "selected": selected });
        if let Some(custom) = custom {
            answer["custom"] = Value::String(custom);
        }
        out.push(answer);
    }
    json!({ "answers": out })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw_catalog() -> Value {
        json!({ "default": { "provider": "deepseek", "model": "deepseek-chat" },
                "groups": [{ "provider": "deepseek", "models": [{ "id": "deepseek-chat" }] }] })
    }

    #[test]
    fn catalog_without_preset_roster_has_no_modes() {
        let c = map_catalog(&raw_catalog(), None);
        assert!(c.modes.is_empty());
        assert_eq!(c.defaults.mode, None);
        let c = map_catalog(&raw_catalog(), Some(&json!({})));
        assert!(c.modes.is_empty() && c.defaults.mode.is_none());
    }

    #[test]
    fn catalog_modes_come_from_the_preset_roster() {
        let roster = json!({ "presets": [
            { "id": "coder", "isDefault": true, "name": "Coder", "description": "Codes" },
            { "id": "plain", "isDefault": false },
            { "id": "bad", "isDefault": false, "broken": "row failed" },
        ]});
        let c = map_catalog(&raw_catalog(), Some(&roster));
        let ids: Vec<_> = c.modes.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["coder", "plain"]);
        assert_eq!(c.modes[0].name, "Coder");
        assert_eq!(c.modes[1].name, "plain");
        assert_eq!(c.defaults.mode.as_deref(), Some("coder"));
        let none_default = json!({ "presets": [{ "id": "a", "isDefault": false }] });
        assert_eq!(
            map_catalog(&raw_catalog(), Some(&none_default))
                .defaults
                .mode,
            None
        );
    }

    #[test]
    fn maps_deepseek_session_summary() {
        let raw = json!({
            "sessionId": "ses_abc123",
            "cwd": "/home/ryu/Work",
            "running": true,
            "updatedAt": 1789700000000u64,
            "projections": {
                "values": {
                    "title": "My Work Session",
                    "modelSelection": {
                        "next": {
                            "provider": "deepseek",
                            "model": "deepseek-chat",
                            "reasoningEffort": "low"
                        }
                    },
                    "tokenUsage": {
                        "uncachedInputTokens": 100,
                        "outputTokens": 20,
                        "cacheReadTokens": 50,
                        "cacheWriteTokens": 0
                    }
                }
            }
        });

        let session = map_session(&raw).unwrap();
        assert_eq!(session.asid.0, "ses_abc123");
        assert_eq!(session.title, "My Work Session");
        assert_eq!(session.directory.as_deref(), Some("/home/ryu/Work"));
        assert_eq!(session.status, AgentSessionStatus::Busy);
        assert_eq!(
            session.model,
            Some(ModelRef {
                provider_id: "deepseek".into(),
                model_id: "deepseek-chat".into(),
                variant: Some("low".into()),
            })
        );
        let tok = session.tokens.unwrap();
        assert_eq!(tok.input, 150);
        assert_eq!(tok.output, 20);
    }

    /// A page as `SessionHistoryController.page` records it: `user/message`
    /// data is the `UserMessage`, `assistant/message` wraps its message and
    /// stream, `tool/call` repeats the call and `tool/result` answers it
    /// (shapes from dsh-session/lib/types/types.d.ts and
    /// dsh-llm/lib/types/message.d.ts).
    fn page() -> Value {
        json!({
            "records": [
                { "type": "event", "event": { "type": "turn/start", "seq": 4, "time": 1790558000000u64, "data": { "turn": 1 } } },
                { "type": "event", "event": {
                    "type": "user/message", "seq": 5, "time": 1790558000100u64, "surfaceOp": "append",
                    "data": { "id": "7d0b0a9c-1f7d-4e2d-9d3c-1c0e6a5b4f3a", "role": "user",
                              "content": [ { "type": "text", "text": "Hello DeepSeek" } ],
                              "source": { "kind": "user", "rpcId": "req_1" } } } },
                { "type": "event", "event": {
                    "type": "assistant/message", "seq": 8, "time": 1790558001000u64, "surfaceOp": "append",
                    "data": { "turn": 1, "step": 1,
                              "message": { "id": "2b6f1e44-7c0d-4b1e-8f4a-9e2d1c3b5a6f", "role": "assistant",
                                           "content": [
                                               { "type": "reasoning", "text": "thinking..." },
                                               { "type": "text", "text": "Listing." },
                                               { "type": "tool-call", "id": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}" }
                                           ],
                                           "source": { "kind": "model", "provider": "deepseek", "model": "deepseek-chat" } },
                              "stream": [],
                              "usage": { "inputTokens": 10, "outputTokens": 5, "cacheReadTokens": 2 } } } },
                { "type": "event", "event": { "type": "tool/call", "seq": 9, "time": 1790558001100u64,
                    "data": { "turn": 1, "step": 1, "callId": "call_1", "name": "bash", "arguments": "{\"command\":\"ls\"}" } } },
                { "type": "event", "event": { "type": "tool/result", "seq": 10, "time": 1790558002000u64, "surfaceOp": "append",
                    "data": { "turn": 1, "step": 1,
                              "message": { "id": "c1", "role": "tool", "toolCallId": "call_1",
                                           "content": [ { "type": "text", "text": "a.rs\nb.rs" } ],
                                           "source": { "kind": "tool" } } } } },
                { "type": "event", "event": { "type": "turn/end", "seq": 11, "time": 1790558002500u64, "data": { "turn": 1, "reason": { "kind": "completed" } } } }
            ]
        })
    }

    #[test]
    fn maps_deepseek_timeline_records() {
        let items = map_timeline_records(&page());
        assert_eq!(
            items.len(),
            4,
            "one user row, reasoning, text and a tool card"
        );

        assert_eq!(items[0].role, TimelineRole::User);
        assert_eq!(
            items[0].message_id, "s000000000005",
            "message ids are the log seq"
        );
        assert_eq!(items[0].id, text_item_id("s000000000005", 0));
        assert_eq!(
            items[0].part,
            AgentPart::Text {
                text: "Hello DeepSeek".into()
            }
        );

        assert_eq!(items[1].role, TimelineRole::Assistant);
        assert_eq!(items[1].message_id, "s000000000008");
        assert_eq!(items[1].id, reasoning_item_id("s000000000008", 0));
        assert_eq!(items[2].id, text_item_id("s000000000008", 1));
        assert_eq!(
            items[2].part,
            AgentPart::Text {
                text: "Listing.".into()
            }
        );

        assert_eq!(items[3].id, tool_item_id("s000000000008", "call_1"));
        let AgentPart::Tool(ref call) = items[3].part else {
            panic!("a tool card");
        };
        assert_eq!(call.name, "bash");
        assert_eq!(
            call.input,
            json!({ "command": "ls" }),
            "arguments are parsed from the raw JSON string"
        );
        assert_eq!(
            call.state,
            ToolCallStatus::Completed,
            "the tool/result in the page settles it"
        );
        assert_eq!(call.output, Some(json!("a.rs\nb.rs")));
        assert_eq!(call.time.completed, Some(1790558002000));
        assert_eq!(items[3].ordinal, 2);

        // Rows sort by message id, so the log order is the display order.
        let mut sorted = items.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        assert_eq!(
            sorted.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn harness_injected_user_messages_are_synthetic_system_rows_not_user_rows() {
        let injected = |seq: u64, kind: &str, text: &str| {
            json!({ "type": "user/message", "seq": seq, "time": 5, "surfaceOp": "append",
                "data": { "id": "m", "role": "user", "content": [ { "type": "text", "text": text } ],
                          "source": { "kind": kind, "form": "snapshot" } } })
        };
        let typed = json!({ "type": "user/message", "seq": 1, "time": 5, "surfaceOp": "append",
            "data": { "id": "u", "role": "user", "content": [ { "type": "text", "text": "hi" } ],
                      "source": { "kind": "user", "rpcId": "req_1", "clientTimeZone": "UTC" } } });
        let page = json!({ "records": [
            { "event": typed },
            { "event": injected(2, "runtime-context", "Current runtime context. This snapshot supersedes earlier runtime-context ...") },
            { "event": injected(3, "skill-catalog", "<system-reminder>\nA skill is a reusable set of task-specific instructions ...") },
            { "event": injected(4, "time-context", "The current time is ...") },
        ] });
        let items = map_timeline_records(&page);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].role, TimelineRole::User);
        assert!(matches!(items[0].part, AgentPart::Text { ref text } if text == "hi"));
        for (item, kind) in
            items[1..]
                .iter()
                .zip(["runtime-context", "skill-catalog", "time-context"])
        {
            assert_eq!(item.role, TimelineRole::System);
            assert!(
                matches!(&item.part, AgentPart::Synthetic { description: Some(d), .. } if d == kind),
                "{:?}",
                item.part
            );
        }
        // Same result for a single live record.
        let live = map_record(&injected(9, "skill-invocation", "<skill_content>"));
        assert_eq!(live[0].role, TimelineRole::System);
        // A record without a source stays a user message.
        let bare = json!({ "type": "user/message", "seq": 1, "time": 5,
            "data": { "role": "user", "content": [ { "type": "text", "text": "x" } ] } });
        assert_eq!(map_record(&bare)[0].role, TimelineRole::User);
    }

    #[test]
    fn an_unanswered_tool_call_is_running_and_a_failed_result_is_failed() {
        let event = json!({ "type": "assistant/message", "seq": 3, "time": 7, "data": { "message": {
            "role": "assistant", "content": [ { "type": "tool-call", "id": "c9", "name": "edit", "arguments": "{\"path\":\"a\"" } ] } } });
        let items = map_record(&event);
        let AgentPart::Tool(ref call) = items[0].part else {
            panic!()
        };
        assert_eq!(call.state, ToolCallStatus::Running);
        assert_eq!(
            call.input,
            json!({ "raw": "{\"path\":\"a\"" }),
            "a truncated argument string is kept"
        );

        let result = map_tool_result(&json!({
            "message": { "role": "tool", "toolCallId": "c9", "content": [ { "type": "text", "text": "boom" } ], "isError": true },
            "error": { "name": "ToolError", "code": "EACCES", "reason": "permission denied" }
        }))
        .unwrap();
        assert!(result.is_error);
        assert_eq!(result.error.as_ref().unwrap().message, "permission denied");
        let mut call = call.clone();
        apply_tool_result(&mut call, &result, 9);
        assert_eq!(call.state, ToolCallStatus::Failed);
        assert_eq!(call.output, Some(json!("boom")));
    }

    #[test]
    fn a_provisional_message_id_sorts_between_the_seqs_it_belongs_to() {
        let live = provisional_message_id(41, "attempt-1");
        assert!(record_message_id(41) < live);
        assert!(live < record_message_id(42));
        assert!(
            record_message_id(9) < record_message_id(10),
            "zero padding keeps numeric order"
        );
    }

    #[test]
    fn turn_end_reasons_become_statuses() {
        assert_eq!(
            map_turn_end(&json!({ "reason": { "kind": "completed" } })).0,
            AgentSessionStatus::Idle
        );
        assert_eq!(
            map_turn_end(&json!({ "reason": { "kind": "aborted", "reason": "cancel" } })).0,
            AgentSessionStatus::Interrupted
        );
        let (status, error) = map_turn_end(
            &json!({ "reason": { "kind": "error", "error": { "code": "RATE_LIMIT", "message": "slow down" } } }),
        );
        assert_eq!(status, AgentSessionStatus::Failed);
        assert_eq!(error.unwrap().message, "slow down");
        let usage = map_usage(&json!({ "inputTokens": 10, "outputTokens": 3, "cacheReadTokens": 5, "reasoningTokens": 1 })).unwrap();
        assert_eq!(
            (usage.input, usage.output, usage.reasoning),
            (15, 3, Some(1))
        );
    }

    #[test]
    fn a_snapshot_header_and_projections_become_session_info() {
        let info = map_snapshot_session(
            "ses_1",
            &json!({ "version": 4, "id": "ses_1", "createdAt": 1, "cwd": "/w", "parentSession": "ses_0", "isSeeded": true, "agentPreset": "coder" }),
            &json!({ "asOfSeq": 12, "values": { "title": "Fix the build" } }),
        )
        .unwrap();
        assert_eq!(info.title, "Fix the build");
        assert_eq!(info.directory.as_deref(), Some("/w"));
        assert_eq!(info.parent_id.as_deref(), Some("ses_0"));
        assert_eq!(info.mode.as_deref(), Some("coder"));
    }

    /// The waterfall request is `ApprovalRequestEvent` less its `agent` and
    /// `signal` (`projectRemoteEventRequest` in dsh-api-gateway).
    #[test]
    fn an_approval_request_becomes_a_two_option_permission() {
        let request = map_approval_request(
            "evt-1",
            "ses_1",
            &json!({
                "toolName": "bash", "callId": "call_1", "reason": "hook: the command deletes files",
                "displayReason": { "en": "Delete the build directory?", "zh": "..." }
            }),
        );
        assert_eq!(request.id, "evt-1");
        assert_eq!(request.asid.0, "ses_1");
        assert_eq!(request.action, "bash");
        assert_eq!(request.prompt, "Delete the build directory?");
        assert_eq!(
            request.message.as_deref(),
            Some("hook: the command deletes files")
        );
        assert_eq!(request.source_tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            request
                .options
                .iter()
                .map(|o| o.decision)
                .collect::<Vec<_>>(),
            vec![PermissionDecision::Allow, PermissionDecision::Deny],
            "the outcome vocabulary has no always"
        );
        assert_eq!(
            map_approval_request("e", "s", &json!({ "toolName": "web" })).prompt,
            "Allow web?"
        );
        assert_eq!(approval_outcome(PermissionDecision::Allow), "allowed-once");
        assert_eq!(
            approval_outcome(PermissionDecision::AllowAlways),
            "allowed-once"
        );
        assert_eq!(approval_outcome(PermissionDecision::Deny), "rejected");
    }

    /// `AskUserQuestionItem[]` from dsh-user-questions/lib/types/types.d.ts.
    fn questions() -> Value {
        json!({ "questions": [
            { "id": "mode", "question": "Which mode?", "header": "Choose Mode",
              "options": [ { "label": "Fast (Recommended)", "description": "no tests" }, { "label": "Careful" } ] },
            { "id": "tags", "question": "Tags?", "multiSelect": true,
              "options": [ { "label": "a" }, { "label": "b" } ] },
            { "id": "name", "question": "Project name?", "detail": "Used in the manifest." }
        ] })
    }

    #[test]
    fn a_question_batch_becomes_a_form_and_its_answers_go_back_by_label() {
        let form = map_question_request("evt-2", "ses_1", &questions()).unwrap();
        assert_eq!(form.id, "evt-2");
        assert_eq!(form.title, "Choose Mode");
        assert_eq!(form.fields.len(), 3);
        match &form.fields[0] {
            FormField::String {
                key,
                options,
                custom,
                ..
            } => {
                assert_eq!(key, "mode");
                assert_eq!(options[0].value, "Fast (Recommended)");
                assert!(custom, "a free-text answer is allowed beside the options");
            }
            other => panic!("expected a string field, got {other:?}"),
        }
        assert!(matches!(
            &form.fields[1],
            FormField::Multiselect { key, options, .. } if key == "tags" && options.len() == 2
        ));
        match &form.fields[2] {
            FormField::String {
                options,
                description,
                ..
            } => {
                assert!(options.is_empty());
                assert_eq!(description.as_deref(), Some("Used in the manifest."));
            }
            other => panic!("expected a free-text field, got {other:?}"),
        }
        assert!(map_question_request("e", "s", &json!({ "questions": [] })).is_none());

        let answers = question_answers(
            &questions(),
            &json!({
                "mode": "Careful", "tags": ["a", "b"], "name": "muqun"
            }),
        );
        assert_eq!(
            answers,
            json!({ "answers": [
                { "id": "mode", "selected": ["Careful"] },
                { "id": "tags", "selected": ["a", "b"] },
                { "id": "name", "selected": [], "custom": "muqun" }
            ] })
        );
        // A value that names no option is the "Other" free text.
        let answers = question_answers(&questions(), &json!({ "mode": "Both" }));
        assert_eq!(
            answers["answers"][0],
            json!({ "id": "mode", "selected": [], "custom": "Both" })
        );
    }
}
