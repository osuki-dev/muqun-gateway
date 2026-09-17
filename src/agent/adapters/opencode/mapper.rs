use serde_json::Value;
use crate::agent::domain::{
    part_item_id, reasoning_item_id, text_item_id, tool_item_id, AgentErrorInfo, AgentInfo,
    AgentPart, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, CompactionStatus,
    FormField, FormOption, FormRequest, McpServerInfo, ModelInfo, ModelRef, ModelVariantInfo,
    PermissionDecision, PermissionOption, PermissionRequest, SessionForkInfo, SessionRevertInfo,
    CatalogDefaults, CommandInfo, ProviderInfo, ProviderModelInfo, SkillInfo, TimelineItem,
    TimelineRole, TodoItem, TokensUsage, ToolCall, ToolCallStatus, ToolTime,
};

/// `Model.Ref` as v2 spells it: `{id, providerID, variant?}`. `modelID` is
/// accepted as a v1-compat spelling of `id`.
pub fn map_model_ref(val: &Value) -> Option<ModelRef> {
    let model_id = val
        .get("id")
        .or_else(|| val.get("modelID"))
        .and_then(Value::as_str)?;
    let provider_id = val
        .get("providerID")
        .or_else(|| val.get("providerId"))
        .and_then(Value::as_str)
        .unwrap_or("opencode");
    Some(ModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        variant: val.get("variant").and_then(Value::as_str).map(str::to_string),
    })
}

/// `Session.StructuredError {type, message, status?}`.
pub fn map_error(val: &Value) -> Option<AgentErrorInfo> {
    let obj = val.as_object()?;
    let name = obj
        .get("type")
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_string();
    let message = obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if message.is_empty() && name == "error" {
        return None;
    }
    Some(AgentErrorInfo {
        name,
        message,
        status: obj
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|s| u16::try_from(s).ok()),
    })
}

fn map_tokens(val: &Value) -> TokensUsage {
    TokensUsage {
        input: val.get("input").and_then(Value::as_u64).unwrap_or(0),
        output: val.get("output").and_then(Value::as_u64).unwrap_or(0),
        reasoning: val.get("reasoning").and_then(Value::as_u64),
        cache_read: val.pointer("/cache/read").and_then(Value::as_u64),
        cache_write: val.pointer("/cache/write").and_then(Value::as_u64),
    }
}

fn map_revert(val: &Value) -> Option<SessionRevertInfo> {
    let message_id = val.get("messageID").and_then(Value::as_str)?;
    Some(SessionRevertInfo {
        message_id: message_id.to_string(),
        part_id: val.get("partID").and_then(Value::as_str).map(str::to_string),
        snapshot: val.get("snapshot").and_then(Value::as_str).map(str::to_string),
        files: val.get("files").cloned(),
    })
}

fn map_fork(val: &Value) -> Option<SessionForkInfo> {
    let session_id = val.get("sessionID").and_then(Value::as_str)?;
    Some(SessionForkInfo {
        session_id: session_id.to_string(),
        boundary_type: val
            .pointer("/boundary/type")
            .and_then(Value::as_str)
            .map(str::to_string),
        message_id: val
            .pointer("/boundary/messageID")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub fn map_session(val: &Value) -> Option<AgentSessionInfo> {
    let item = val.get("data").unwrap_or(val);
    let id = item.get("id").and_then(Value::as_str)?;
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();

    let agent = item.get("agent").and_then(Value::as_str).map(str::to_string);

    // No fabricated default: a session whose model OpenCode has not reported
    // is `None`, and the app shows whatever OpenCode resolves at run time.
    let model = item.get("model").and_then(map_model_ref);

    let cost = item.get("cost").and_then(Value::as_f64);
    let tokens = item.get("tokens").map(map_tokens);
    let limit = item.get("limit").cloned();

    let directory = item
        .pointer("/location/directory")
        .or_else(|| item.get("directory"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let updated_ms = item
        .pointer("/time/updated")
        .or_else(|| item.pointer("/time/created"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let parent_id = item
        .get("parentID")
        .or_else(|| item.get("parentId"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let project_id = item
        .get("projectID")
        .or_else(|| item.get("projectId"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let outcome = item.get("outcome").and_then(Value::as_str).map(str::to_string);
    // v2 has no session status field; the last outcome is the closest thing a
    // read of `Session.Info` can say, and the event stream corrects it live.
    let status = match outcome.as_deref() {
        Some("failed") => AgentSessionStatus::Failed,
        Some("interrupted") => AgentSessionStatus::Interrupted,
        _ => AgentSessionStatus::Idle,
    };

    Some(AgentSessionInfo {
        asid: AgentSessionId(id.to_string()),
        backend_session_id: id.to_string(),
        title,
        agent,
        model,
        status,
        directory,
        cost,
        tokens,
        limit,
        parent_id,
        project_id,
        outcome,
        error: None,
        revert: item.get("revert").and_then(map_revert),
        fork: item.get("fork").and_then(map_fork),
        time_idle: item.pointer("/time/idle").and_then(Value::as_u64),
        time_viewed: item.pointer("/time/viewed").and_then(Value::as_u64),
        deleted: false,
        updated_ms,
    })
}

pub fn map_project(val: &Value) -> Option<AgentProject> {
    let item = val.get("data").unwrap_or(val);
    let id = item.get("id").and_then(Value::as_str)?;
    let canonical = item.get("canonical").and_then(Value::as_str).unwrap_or(id);
    let vcs = item.get("vcs").and_then(Value::as_str).map(str::to_string);
    let name = if id == "global" || canonical == "/" {
        "Global Root".to_string()
    } else {
        std::path::Path::new(canonical)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(canonical)
            .to_string()
    };
    let sandboxes = item
        .get("sandboxes")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    Some(AgentProject {
        id: id.to_string(),
        canonical: canonical.to_string(),
        name,
        vcs,
        sandboxes,
    })
}

/// Assistant prose that only repeats the tool result immediately above it.
///
/// Only an exact match counts: the previous rule also dropped text that merely
/// *contained* or was contained by the output, which threw away real prose
/// whenever a tool returned something short.
fn is_duplicate_tool_text(text: &str, prev_item: Option<&TimelineItem>) -> bool {
    let Some(prev) = prev_item else {
        return false;
    };
    let AgentPart::Tool(ref call) = prev.part else {
        return false;
    };

    let clean = |s: &str| {
        s.trim()
            .trim_start_matches("```")
            .trim_end_matches("```")
            .replace("Command exited with code 0.", "")
            .replace("Command exited with code 0", "")
            .trim()
            .to_string()
    };

    let clean_text = clean(text);
    if clean_text.is_empty() {
        return true;
    }
    let tool_out = match call.output {
        Some(Value::String(ref s)) => s.clone(),
        Some(ref other) => other.to_string(),
        None => String::new(),
    };
    !tool_out.is_empty() && clean_text == clean(&tool_out)
}

fn message_attachments(msg: &Value) -> Option<Vec<String>> {
    msg.get("files")
        .or_else(|| msg.get("attachments"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    if let Some(s) = f.as_str() {
                        return Some(s.to_string());
                    }
                    f.get("uri")
                        .or_else(|| f.pointer("/source/uri"))
                        .or_else(|| f.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
}

/// Turn OpenCode's flat list of typed messages into timeline items.
///
/// v2 messages are `user | assistant | compaction | skill | shell |
/// model-switched | agent-switched | synthetic | system | location-switched`.
/// Everything but `assistant` is a single row; `assistant` expands into its
/// `content` array.
pub fn map_messages_to_timeline(messages: &[Value], asid: &AgentSessionId) -> Vec<TimelineItem> {
    let mut items: Vec<TimelineItem> = Vec::new();
    let mut seq = 1u64;

    // The list arrives in the order the caller asked for (`order=asc`); the
    // gateway does not re-derive it from timestamps.
    for msg in messages {
        for mut item in map_message(msg, asid) {
            if let AgentPart::Text { ref text } = item.part {
                if is_duplicate_tool_text(text, items.last()) {
                    continue;
                }
            }
            item.seq = seq;
            seq += 1;
            items.push(item);
        }
    }

    items
}

/// One message, expanded into the rows it contributes.
pub fn map_message(msg: &Value, asid: &AgentSessionId) -> Vec<TimelineItem> {
    let msg_id = msg.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let msg_type = msg.get("type").and_then(Value::as_str).unwrap_or("assistant");
    let updated_ms = msg
        .pointer("/time/completed")
        .or_else(|| msg.pointer("/time/streamed"))
        .or_else(|| msg.pointer("/time/created"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let role = match msg_type {
        "user" => TimelineRole::User,
        "assistant" => TimelineRole::Assistant,
        _ => TimelineRole::System,
    };

    let mut out: Vec<TimelineItem> = Vec::new();
    macro_rules! push {
        ($id:expr, $ordinal:expr, $part:expr, $attachments:expr $(,)?) => {
            out.push(TimelineItem {
                id: $id,
                message_id: msg_id.to_string(),
                role,
                part: $part,
                seq: 0,
                updated_ms,
                ordinal: $ordinal,
                attachments: $attachments,
            })
        };
    }

    match msg_type {
        "user" | "synthetic" | "system" => {
            let text = msg.get("text").and_then(Value::as_str).unwrap_or("");
            let description = msg
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            if !text.trim().is_empty() {
                let part = match msg_type {
                    "synthetic" => AgentPart::Synthetic {
                        text: text.to_string(),
                        description,
                    },
                    "system" => AgentPart::System {
                        text: text.to_string(),
                        description,
                    },
                    _ => AgentPart::Text {
                        text: text.to_string(),
                    },
                };
                push!(text_item_id(msg_id, 0), 0, part, message_attachments(msg));
            }
        }
        "compaction" => {
            push!(
                part_item_id(msg_id, 0),
                0,
                map_compaction_message(msg),
                None,
            );
        }
        "skill" => {
            push!(
                part_item_id(msg_id, 0),
                0,
                AgentPart::Skill {
                    skill: msg.get("skill").and_then(Value::as_str).unwrap_or("").to_string(),
                    name: msg.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    text: msg.get("text").and_then(Value::as_str).unwrap_or("").to_string(),
                },
                None,
            );
        }
        "shell" => {
            push!(part_item_id(msg_id, 0), 0, map_shell_message(msg), None);
        }
        "model-switched" => {
            if let Some(model) = msg.get("model").and_then(map_model_ref) {
                push!(
                    part_item_id(msg_id, 0),
                    0,
                    AgentPart::ModelSwitched {
                        model,
                        previous: msg.get("previous").and_then(map_model_ref),
                    },
                    None,
                );
            }
        }
        "agent-switched" => {
            if let Some(agent) = msg.get("agent").and_then(Value::as_str) {
                push!(
                    part_item_id(msg_id, 0),
                    0,
                    AgentPart::AgentSwitched {
                        agent: agent.to_string(),
                        previous: msg.get("previous").and_then(Value::as_str).map(str::to_string),
                    },
                    None,
                );
            }
        }
        "location-switched" => {
            if let Some(dir) = msg.pointer("/location/directory").and_then(Value::as_str) {
                push!(
                    part_item_id(msg_id, 0),
                    0,
                    AgentPart::LocationSwitched {
                        directory: dir.to_string(),
                        previous: msg
                            .pointer("/previous/location/directory")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    },
                    None,
                );
            }
        }
        _ => {}
    }

    // `user` messages also carry an explicit top-level text, handled above.
    // `assistant` content is the interleaved text/reasoning/tool array.
    let attachments = message_attachments(msg);
    if msg_type == "assistant" || msg_type == "user" {
        // `content` is the v2 name and `parts` the v1 one.
        let content_parts = msg
            .get("content")
            .or_else(|| msg.get("parts"))
            .and_then(Value::as_array);
        if let Some(parts) = content_parts {
            let mut text_n = 0u64;
            let mut reasoning_n = 0u64;
            for (idx, part) in parts.iter().enumerate() {
                let Some(agent_part) = map_part(part, role, asid) else {
                    continue;
                };
                let ordinal = idx as u64;
                let id = match &agent_part {
                    AgentPart::Text { .. } => {
                        let id = text_item_id(msg_id, text_n);
                        text_n += 1;
                        id
                    }
                    AgentPart::Reasoning { .. } => {
                        let id = reasoning_item_id(msg_id, reasoning_n);
                        reasoning_n += 1;
                        id
                    }
                    AgentPart::Tool(call) => tool_item_id(msg_id, &call.id),
                    _ => part_item_id(msg_id, ordinal),
                };
                if out.iter().any(|existing| existing.id == id) {
                    continue;
                }
                let att = if idx == 0 { attachments.clone() } else { None };
                push!(id, ordinal, agent_part, att);
            }
        }
    }

    out
}

fn map_compaction_message(msg: &Value) -> AgentPart {
    let status = match msg.get("status").and_then(Value::as_str) {
        Some("completed") => CompactionStatus::Completed,
        Some("failed") => CompactionStatus::Failed,
        _ => CompactionStatus::Running,
    };
    AgentPart::Compaction {
        status,
        reason: msg.get("reason").and_then(Value::as_str).map(str::to_string),
        summary: msg.get("summary").and_then(Value::as_str).map(str::to_string),
        recent: msg.get("recent").and_then(Value::as_str).map(str::to_string),
        tokens: msg.get("tokens").map(map_tokens),
        cost: msg.get("cost").and_then(Value::as_f64),
        error: msg.get("error").and_then(map_error),
    }
}

fn map_shell_message(msg: &Value) -> AgentPart {
    AgentPart::Shell {
        shell_id: msg.get("shellID").and_then(Value::as_str).unwrap_or("").to_string(),
        command: msg.get("command").and_then(Value::as_str).unwrap_or("").to_string(),
        status: msg
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_string(),
        exit: msg.get("exit").and_then(Value::as_f64),
        output: msg
            .pointer("/output/output")
            .and_then(Value::as_str)
            .map(str::to_string),
        truncated: msg
            .pointer("/output/truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn extract_todo_items(val: &Value) -> Option<Vec<TodoItem>> {
    let read_items = |arr: &Vec<Value>| -> Vec<TodoItem> {
        arr.iter()
            .filter_map(|it| {
                let text = it
                    .get("content")
                    .or_else(|| it.get("text"))
                    .or_else(|| it.get("title"))
                    .and_then(Value::as_str)?
                    .to_string();
                let done = it.get("status").and_then(Value::as_str) == Some("completed")
                    || it.get("done").and_then(Value::as_bool).unwrap_or(false);
                Some(TodoItem { text, done })
            })
            .collect()
    };

    let raw_items = val
        .get("items")
        .or_else(|| val.pointer("/state/input/todos"))
        .or_else(|| val.pointer("/input/todos"))
        .or_else(|| val.pointer("/state/metadata/todos"))
        .or_else(|| val.get("todos"));

    if let Some(arr) = raw_items.and_then(Value::as_array) {
        let items = read_items(arr);
        if !items.is_empty() {
            return Some(items);
        }
    }

    let output = val.pointer("/state/output").or_else(|| val.get("output"));
    if let Some(arr) = output.and_then(Value::as_array) {
        let items = read_items(arr);
        if !items.is_empty() {
            return Some(items);
        }
    } else if let Some(s) = output.and_then(Value::as_str) {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(s) {
            let items = read_items(&arr);
            if !items.is_empty() {
                return Some(items);
            }
        }
    }

    None
}

/// Flatten `Tool.Content[]` into the single string the previous release's
/// `output` field carried. File items are represented by their uri, because a
/// client that only reads `output` still needs to know something was returned.
pub fn tool_content_to_output(content: &Value) -> Option<Value> {
    match content {
        Value::Array(arr) => {
            let mut parts = Vec::new();
            let mut all_known = true;
            for item in arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                } else if let Some(s) = item.as_str() {
                    parts.push(s.to_string());
                } else if item.get("type").and_then(Value::as_str) == Some("file") {
                    if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                        parts.push(uri.to_string());
                    }
                } else {
                    all_known = false;
                    break;
                }
            }
            if all_known && !parts.is_empty() {
                Some(Value::String(parts.join("\n")))
            } else {
                Some(content.clone())
            }
        }
        Value::String(s) => {
            if let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(s) {
                tool_content_to_output(&parsed)
            } else {
                Some(Value::String(s.clone()))
            }
        }
        Value::Null => None,
        other => Some(other.clone()),
    }
}

fn normalize_tool_output(output: Option<Value>) -> Option<Value> {
    tool_content_to_output(&output?)
}

/// A card header for a tool call. OpenCode has no `title`, so this mirrors what
/// the TUI does: the tool name plus the most identifying part of its input.
pub fn tool_title(name: &str, input: &Value) -> Option<String> {
    let s = |key: &str| input.get(key).and_then(Value::as_str);
    let title = match name {
        "read" | "write" | "edit" | "patch" => s("path")
            .map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p)
                    .to_string()
            })?,
        "shell" | "bash" => s("command")?.lines().next().unwrap_or("").to_string(),
        "glob" => s("pattern")?.to_string(),
        "grep" | "search" => s("pattern").or_else(|| s("query"))?.to_string(),
        "subagent" | "task" => {
            let agent = s("agent").unwrap_or("agent");
            let description = s("description").unwrap_or("");
            if description.is_empty() {
                agent.to_string()
            } else {
                format!("{agent}: {description}")
            }
        }
        "skill" => s("id").or_else(|| s("skill"))?.to_string(),
        "webfetch" => s("url")?.to_string(),
        _ => return None,
    };
    if title.trim().is_empty() {
        None
    } else {
        Some(title)
    }
}

fn tool_state_from_str(status: Option<&str>) -> ToolCallStatus {
    match status {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed") | Some("error") => ToolCallStatus::Failed,
        Some("streaming") => ToolCallStatus::Streaming,
        Some("pending") => ToolCallStatus::Pending,
        _ => ToolCallStatus::Running,
    }
}

/// Fill in everything that is derived from `metadata`: the truncation flag, a
/// backgrounded shell, and the child session of a `subagent` call.
pub fn apply_tool_metadata(call: &mut ToolCall) {
    let Some(ref metadata) = call.metadata else {
        return;
    };
    if metadata.get("truncated").and_then(Value::as_bool) == Some(true) {
        call.truncated = true;
    }
    if metadata.get("background").and_then(Value::as_bool) == Some(true) {
        call.background = true;
    }
    if let Some(child) = metadata.get("sessionID").and_then(Value::as_str) {
        call.child_session_id = Some(child.to_string());
    }
}

/// A tool part out of an assistant message's `content` array.
pub fn map_tool_call(val: &Value) -> ToolCall {
    let id = val.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let name = val
        .get("name")
        .or_else(|| val.get("tool"))
        .and_then(Value::as_str)
        .unwrap_or("tool");

    let state = val.get("state");
    let status = tool_state_from_str(state.and_then(|s| s.get("status")).and_then(Value::as_str));

    let input = state
        .and_then(|s| s.get("input"))
        .or_else(|| val.get("input"))
        .cloned()
        .unwrap_or(Value::Null);

    let content = state
        .and_then(|s| s.get("content"))
        .or_else(|| val.get("content"))
        .cloned();
    let raw_output = state
        .and_then(|s| s.get("output"))
        .or_else(|| val.get("output"))
        .cloned()
        .or_else(|| content.clone());

    let metadata = state
        .and_then(|s| s.get("metadata"))
        .or_else(|| val.get("metadata"))
        .cloned()
        .filter(|m| !m.is_null());

    let time = ToolTime {
        created: val.pointer("/time/created").and_then(Value::as_u64),
        ran: val.pointer("/time/ran").and_then(Value::as_u64),
        completed: val.pointer("/time/completed").and_then(Value::as_u64),
    };

    let mut call = ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        title: tool_title(name, &input),
        input,
        output: normalize_tool_output(raw_output),
        content,
        metadata,
        state: status,
        status,
        error: state.and_then(|s| s.get("error")).and_then(map_error),
        child_session_id: None,
        background: false,
        truncated: false,
        time,
    };
    apply_tool_metadata(&mut call);
    call
}

pub fn map_part(val: &Value, _role: TimelineRole, asid: &AgentSessionId) -> Option<AgentPart> {
    let part_type = val.get("type").and_then(Value::as_str).unwrap_or("");
    match part_type {
        "text" => {
            let text = val
                .get("text")
                .or_else(|| val.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if text.is_empty() {
                None
            } else {
                Some(AgentPart::Text {
                    text: text.to_string(),
                })
            }
        }
        "reasoning" => {
            let mut text = val
                .get("text")
                .or_else(|| val.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                if let Some(details) = val.pointer("/state/reasoningDetails").and_then(Value::as_array) {
                    for d in details {
                        if let Some(t) = d.get("text").and_then(Value::as_str) {
                            text.push_str(t);
                        }
                    }
                }
            }
            if text.trim().is_empty() {
                return None;
            }
            let duration_ms = val.get("durationMs").and_then(Value::as_u64).or_else(|| {
                let created = val.pointer("/time/created").and_then(Value::as_u64)?;
                let completed = val.pointer("/time/completed").and_then(Value::as_u64)?;
                completed.checked_sub(created)
            });
            Some(AgentPart::Reasoning { text, duration_ms })
        }
        "tool" | "tool_use" | "tool_call" | "tool-call" => {
            let name = val
                .get("name")
                .or_else(|| val.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool");

            // `todowrite` is still folded into a checklist -- it has no other
            // representation. `task`/`subagent` is not: it is a real tool call
            // whose child session the app needs to be able to open.
            if name == "todowrite" || name == "todo" || name == "tasks" {
                if let Some(items) = extract_todo_items(val) {
                    return Some(AgentPart::Todo { items });
                }
            }

            Some(AgentPart::Tool(map_tool_call(val)))
        }
        "compaction" => Some(map_compaction_message(val)),
        "diff" | "patch" => {
            let file = val.get("file").and_then(Value::as_str).unwrap_or("");
            let diff = val
                .get("diff")
                .or_else(|| val.get("patch"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Some(AgentPart::Diff {
                file: file.to_string(),
                diff: diff.to_string(),
            })
        }
        "todo" => {
            let items = extract_todo_items(val)?;
            Some(AgentPart::Todo { items })
        }
        "permission" => {
            let req = map_permission_request(val, asid)?;
            Some(AgentPart::Approval { request: req })
        }
        "form" => {
            let req = map_form_request(val, asid)?;
            Some(AgentPart::Form { request: req })
        }
        _ => None,
    }
}

pub fn map_permission_request(val: &Value, asid: &AgentSessionId) -> Option<PermissionRequest> {
    let id = val.get("id").and_then(Value::as_str)?;
    let action = val
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("permission")
        .to_string();

    let string_list = |key: &str| -> Vec<String> {
        val.get(key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    let resources = string_list("resources");
    // `save` is the set of patterns an "always" reply would persist -- without
    // it the app cannot tell the user what it is about to whitelist.
    let save = string_list("save");
    // v1-compat payloads spell the same thing `patterns`.
    let save = if save.is_empty() {
        string_list("patterns")
    } else {
        save
    };

    let message = val.get("message").and_then(Value::as_str).map(str::to_string);
    // `Permission.Request` has no `tool` field: the tool is `source`, which is
    // `{type:"tool", messageID, id}`. v1-compat payloads do carry `tool`.
    let source_message_id = val
        .pointer("/source/messageID")
        .or_else(|| val.pointer("/tool/messageID"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let source_tool_call_id = val
        .pointer("/source/id")
        .or_else(|| val.pointer("/tool/callID"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let tool = val
        .get("tool")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            val.pointer("/source/type")
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let prompt = if let Some(ref msg) = message {
        msg.clone()
    } else if !resources.is_empty() {
        format!("{action}: {}", resources.join(", "))
    } else {
        format!("Allow {action}?")
    };

    // All three replies are always valid (`Permission.Reply` is a closed enum
    // of `once | always | reject`). What `save` adds is *what* an "always"
    // would whitelist, which the app shows next to the option.
    let options = vec![
        PermissionOption {
            index: 0,
            label: "Allow Once".to_string(),
            decision: PermissionDecision::Allow,
        },
        PermissionOption {
            index: 1,
            label: "Always Allow".to_string(),
            decision: PermissionDecision::AllowAlways,
        },
        PermissionOption {
            index: 2,
            label: "Reject".to_string(),
            decision: PermissionDecision::Deny,
        },
    ];

    Some(PermissionRequest {
        id: id.to_string(),
        asid: asid.clone(),
        action,
        resources,
        save,
        prompt,
        tool,
        source_message_id,
        source_tool_call_id,
        metadata: val.get("metadata").cloned().filter(|m| !m.is_null()),
        message,
        options,
    })
}

fn map_form_conditions(val: &Value) -> Vec<crate::agent::domain::FormWhen> {
    val.get("when")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|w| {
                    Some(crate::agent::domain::FormWhen {
                        key: w.get("key").and_then(Value::as_str)?.to_string(),
                        op: w.get("op").and_then(Value::as_str).unwrap_or("eq").to_string(),
                        value: w.get("value").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn map_form_request(val: &Value, asid: &AgentSessionId) -> Option<FormRequest> {
    let id = val.get("id").and_then(Value::as_str)?;
    let title = val
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Form Request")
        .to_string();

    let raw_fields = val.get("fields").and_then(Value::as_array)?;
    let mut fields = Vec::new();

    for f in raw_fields {
        let key = f.get("key").and_then(Value::as_str).unwrap_or("").to_string();
        let field_title = f
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&key)
            .to_string();
        let description = f.get("description").and_then(Value::as_str).map(str::to_string);
        let required = f.get("required").and_then(Value::as_bool).unwrap_or(false);
        let when = map_form_conditions(f);

        let field_type = f.get("type").and_then(Value::as_str).unwrap_or("string");
        match field_type {
            "string" => {
                let placeholder = f.get("placeholder").and_then(Value::as_str).map(str::to_string);
                let default = f.get("default").and_then(Value::as_str).map(str::to_string);
                let options = parse_form_options(f.get("options"));
                fields.push(FormField::String {
                    key,
                    title: field_title,
                    description,
                    required,
                    when,
                    placeholder,
                    default,
                    options,
                    format: f.get("format").and_then(Value::as_str).map(str::to_string),
                    min_length: f.get("minLength").and_then(Value::as_u64),
                    max_length: f.get("maxLength").and_then(Value::as_u64),
                    pattern: f.get("pattern").and_then(Value::as_str).map(str::to_string),
                    custom: f.get("custom").and_then(Value::as_bool).unwrap_or(false),
                });
            }
            "number" | "integer" => {
                let min = f.get("minimum").and_then(Value::as_f64);
                let max = f.get("maximum").and_then(Value::as_f64);
                let default = f.get("default").and_then(Value::as_f64);
                fields.push(FormField::Number {
                    key,
                    title: field_title,
                    description,
                    required,
                    when,
                    min,
                    max,
                    default,
                });
            }
            "boolean" => {
                let default = f.get("default").and_then(Value::as_bool);
                fields.push(FormField::Boolean {
                    key,
                    title: field_title,
                    description,
                    required,
                    when,
                    default,
                });
            }
            "multiselect" => {
                let options = parse_form_options(f.get("options"));
                let default = f
                    .get("default")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                fields.push(FormField::Multiselect {
                    key,
                    title: field_title,
                    description,
                    required,
                    when,
                    options,
                    default,
                });
            }
            "external" => {
                let url = f.get("url").and_then(Value::as_str).unwrap_or("").to_string();
                fields.push(FormField::External {
                    key,
                    title: field_title,
                    description,
                    when,
                    url,
                });
            }
            _ => {
                fields.push(FormField::Unknown {
                    key,
                    title: field_title,
                    when,
                    raw_type: field_type.to_string(),
                });
            }
        }
    }

    Some(FormRequest {
        id: id.to_string(),
        asid: asid.clone(),
        title,
        fields,
    })
}

fn parse_form_options(val: Option<&Value>) -> Vec<FormOption> {
    val.and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|opt| {
                    let value = opt.get("value").and_then(Value::as_str)?.to_string();
                    let label = opt
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(&value)
                        .to_string();
                    let description = opt
                        .get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    Some(FormOption {
                        value,
                        label,
                        description,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn map_models(data: &[Value]) -> Vec<ModelInfo> {
    data.iter()
        .filter_map(|m| {
            let id = m.get("id").or_else(|| m.get("modelID")).and_then(Value::as_str)?.to_string();
            let name = m.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let provider_id = m
                .get("providerID")
                .and_then(Value::as_str)
                .unwrap_or("opencode")
                .to_string();
            let family = m.get("family").and_then(Value::as_str).map(str::to_string);
            let limit = m.get("limit").cloned();
            let variants = m.get("variants").and_then(Value::as_array).map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v.get("id").and_then(Value::as_str)?.to_string();
                        let reasoning_effort = v
                            .pointer("/settings/reasoningEffort")
                            .or_else(|| v.get("reasoningEffort"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        Some(ModelVariantInfo {
                            id,
                            reasoning_effort,
                        })
                    })
                    .collect()
            });
            let cost = m.get("cost").cloned();
            Some(ModelInfo {
                id,
                name,
                provider_id,
                family,
                limit,
                variants,
                cost,
                enabled: m.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                status: m.get("status").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

pub fn map_agents(data: &[Value]) -> Vec<AgentInfo> {
    data.iter()
        .filter_map(|a| {
            let id = a.get("id").and_then(Value::as_str)?.to_string();
            let name = a.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let description = a.get("description").and_then(Value::as_str).map(str::to_string);
            let mode = a.get("mode").and_then(Value::as_str).map(str::to_string);
            let color = a.get("color").and_then(Value::as_str).map(str::to_string);
            Some(AgentInfo {
                id,
                name,
                description,
                mode,
                color,
                hidden: a.get("hidden").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

pub fn map_mcp(data: &[Value]) -> Vec<McpServerInfo> {
    data.iter()
        .filter_map(|m| {
            let name = m.get("name").and_then(Value::as_str)?.to_string();
            let status = m
                .pointer("/status/status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let error = m
                .pointer("/status/error")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(McpServerInfo { name, status, error })
        })
        .collect()
}

pub fn map_skills(data: &[Value]) -> Vec<SkillInfo> {
    data.iter()
        .filter_map(|s| {
            let id = s.get("id").and_then(Value::as_str)?.to_string();
            let name = s.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            let description = s.get("description").and_then(Value::as_str).unwrap_or("").to_string();
            Some(SkillInfo {
                id,
                name,
                description,
            })
        })
        .collect()
}
/// Group the model list by provider. `/api/provider` describes the providers
/// themselves; the models come from `/api/model`, which is the same list the
/// picker already uses.
pub fn map_providers(data: &[Value], models: &[ModelInfo]) -> Vec<ProviderInfo> {
    let mut providers: Vec<ProviderInfo> = data
        .iter()
        .filter_map(|p| {
            let id = p.get("id").and_then(Value::as_str)?.to_string();
            let name = p.get("name").and_then(Value::as_str).unwrap_or(&id).to_string();
            Some(ProviderInfo {
                id,
                name,
                // Carried through so the app can grey a provider out with a
                // "configure OpenCode on the host" hint. The gateway never
                // writes provider auth.
                activation: p.get("activation").and_then(Value::as_str).map(str::to_string),
                models: Vec::new(),
            })
        })
        .collect();

    for model in models {
        let entry = ProviderModelInfo {
            id: model.id.clone(),
            name: model.name.clone(),
            enabled: model.enabled,
            variants: model.variants.clone().unwrap_or_default(),
            limit: model.limit.clone(),
            status: model.status.clone(),
        };
        match providers.iter_mut().find(|p| p.id == model.provider_id) {
            Some(provider) => provider.models.push(entry),
            None => providers.push(ProviderInfo {
                id: model.provider_id.clone(),
                name: model.provider_id.clone(),
                activation: None,
                models: vec![entry],
            }),
        }
    }

    providers
}

pub fn map_commands(data: &[Value]) -> Vec<CommandInfo> {
    data.iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?.to_string();
            Some(CommandInfo {
                name,
                description: c.get("description").and_then(Value::as_str).map(str::to_string),
                agent: c.get("agent").and_then(Value::as_str).map(str::to_string),
                template: c.get("template").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

/// What OpenCode itself would pick: `GET /api/model/default` for the model and
/// `Config.Info.default_agent` for the agent. `/api/config` answers with the
/// documents the configuration was assembled from, so the last one that names
/// a default wins, which is the merge order OpenCode itself uses.
pub fn map_catalog_defaults(default_model: Option<&Value>, config: &[Value]) -> CatalogDefaults {
    let model = default_model.and_then(map_model_ref).or_else(|| {
        config
            .iter()
            .rev()
            .find_map(|doc| config_model_ref(doc.pointer("/config/model").or_else(|| doc.get("model"))?))
    });

    let agent = config.iter().rev().find_map(|doc| {
        doc.pointer("/config/default_agent")
            .or_else(|| doc.get("default_agent"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    CatalogDefaults { model, agent }
}

/// `Config.Info.model` is either `"provider/model"` (with an optional
/// `#variant`) or `{providerID, model, variant?}`.
fn config_model_ref(val: &Value) -> Option<ModelRef> {
    if let Some(text) = val.as_str() {
        let (provider, rest) = text.split_once('/')?;
        let (model, variant) = match rest.split_once('#') {
            Some((m, v)) => (m, Some(v.to_string())),
            None => (rest, None),
        };
        return Some(ModelRef {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
            variant,
        });
    }
    let provider_id = val.get("providerID").and_then(Value::as_str)?;
    let model_id = val
        .get("model")
        .or_else(|| val.get("id"))
        .and_then(Value::as_str)?;
    Some(ModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        variant: val.get("variant").and_then(Value::as_str).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_map_session() {
        let raw = json!({
            "id": "ses-123",
            "title": "Fix bug in login",
            "agent": "developer",
            "model": {
                "id": "claude-3-5-sonnet",
                "providerID": "anthropic",
                "variant": "high"
            },
            "cost": 0.042,
            "tokens": {
                "input": 1200,
                "output": 350,
                "reasoning": 50
            },
            "location": {
                "directory": "/home/user/project"
            },
            "time": {
                "updated": 1726470000000u64
            }
        });

        let s = map_session(&raw).expect("session should map");
        assert_eq!(s.asid.0, "ses-123");
        assert_eq!(s.title, "Fix bug in login");
        assert_eq!(s.agent.as_deref(), Some("developer"));
        assert_eq!(s.cost, Some(0.042));
        assert_eq!(s.directory.as_deref(), Some("/home/user/project"));
        let model = s.model.expect("model should exist");
        assert_eq!(model.provider_id, "anthropic");
        assert_eq!(model.model_id, "claude-3-5-sonnet");
        assert_eq!(model.variant.as_deref(), Some("high"));
        let tokens = s.tokens.expect("tokens should exist");
        assert_eq!(tokens.input, 1200);
        assert_eq!(tokens.output, 350);
        assert_eq!(tokens.reasoning, Some(50));
    }

    #[test]
    fn test_map_session_with_data_wrapper() {
        let raw = json!({
            "data": {
                "id": "ses-wrapped",
                "title": "Wrapped in data",
                "location": {
                    "directory": "/home/ryu"
                }
            }
        });

        let s = map_session(&raw).expect("wrapped session should map");
        assert_eq!(s.asid.0, "ses-wrapped");
        assert_eq!(s.title, "Wrapped in data");
        assert_eq!(s.directory.as_deref(), Some("/home/ryu"));
    }

    #[test]
    fn test_map_messages_and_tool_calls() {
        let asid = AgentSessionId("ses-1".to_string());
        let messages = vec![
            json!({
                "id": "msg-1",
                "type": "user",
                "time": { "created": 1726470001000u64 },
                "content": [
                    {
                        "type": "text",
                        "text": "Please run cargo check"
                    }
                ]
            }),
            json!({
                "id": "msg-2",
                "type": "assistant",
                "time": { "created": 1726470002000u64 },
                "content": [
                    {
                        "type": "text",
                        "text": "Running cargo check now..."
                    },
                    {
                        "type": "tool_use",
                        "id": "call-1",
                        "name": "bash",
                        "input": {
                            "command": "cargo check"
                        },
                        "state": {
                            "status": "completed",
                            "output": "Finished dev profile"
                        }
                    }
                ]
            })
        ];

        let timeline = map_messages_to_timeline(&messages, &asid);
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0].role, TimelineRole::User);
        assert_eq!(timeline[1].role, TimelineRole::Assistant);
        assert_eq!(timeline[2].role, TimelineRole::Assistant);

        match &timeline[2].part {
            AgentPart::Tool(call) => {
                assert_eq!(call.name, "bash");
                assert_eq!(call.status, ToolCallStatus::Completed);
                assert_eq!(call.state, ToolCallStatus::Completed);
                assert_eq!(call.input.get("command").unwrap(), "cargo check");
                assert_eq!(
                    call.output.as_ref().unwrap().as_str(),
                    Some("Finished dev profile")
                );
                assert_eq!(call.title.as_deref(), Some("cargo check"));
            }
            _ => panic!("expected Tool part"),
        }
    }

    #[test]
    fn test_map_permission_and_form() {
        let asid = AgentSessionId("ses-1".to_string());
        let perm_raw = json!({
            "id": "perm-99",
            "sessionID": "ses-1",
            "action": "execute_command",
            "resources": ["rm -rf /tmp/cache"],
            "message": "Dangerous Command",
        });

        let perm = map_permission_request(&perm_raw, &asid).expect("permission should map");
        assert_eq!(perm.id, "perm-99");
        assert_eq!(perm.asid.0, "ses-1");
        assert_eq!(perm.options.len(), 3);
        assert_eq!(perm.options[0].decision, PermissionDecision::Allow);
        assert_eq!(perm.options[2].decision, PermissionDecision::Deny);

        let form_raw = json!({
            "id": "form-1",
            "sessionID": "ses-1",
            "title": "Config Questions",
            "fields": [
                {
                    "key": "env",
                    "title": "Environment",
                    "type": "string",
                    "required": true,
                    "options": [
                        { "label": "Dev", "value": "development" },
                        { "label": "Prod", "value": "production" }
                    ]
                }
            ]
        });

        let form = map_form_request(&form_raw, &asid).expect("form should map");
        assert_eq!(form.id, "form-1");
        assert_eq!(form.fields.len(), 1);
        match &form.fields[0] {
            FormField::String { key, options, .. } => {
                assert_eq!(key, "env");
                assert_eq!(options.len(), 2);
            }
            _ => panic!("expected string field"),
        }
    }

    #[test]
    fn test_map_session_without_model_reports_none() {
        let raw_without_model = json!({
            "id": "ses-no-model",
            "title": "Untitled",
            "agent": "build",
        });
        let session = map_session(&raw_without_model).expect("should map");
        assert!(
            session.model.is_none(),
            "a session OpenCode reported no model for must not be given one"
        );
    }

    #[test]
    fn test_map_session_reads_model_ref() {
        let raw = json!({
            "id": "ses-model",
            "title": "Titled",
            "model": { "id": "glm-5.3-flash", "providerID": "opencode", "variant": "default" },
        });
        let session = map_session(&raw).expect("should map");
        let model = session.model.expect("model present");
        assert_eq!(model.provider_id, "opencode");
        assert_eq!(model.model_id, "glm-5.3-flash");
        assert_eq!(model.variant.as_deref(), Some("default"));
    }

    #[test]
    fn test_map_todowrite_tool_to_todo_part() {
        let asid = AgentSessionId("ses-1".to_string());
        let part_raw = json!({
            "type": "tool",
            "tool": "todowrite",
            "id": "call-todo",
            "state": {
                "status": "completed",
                "input": {
                    "todos": [
                        { "content": "Step 1: check files", "status": "completed" },
                        { "content": "Step 2: edit files", "status": "in_progress" },
                        { "content": "Step 3: test changes", "status": "pending" }
                    ]
                }
            }
        });

        let part = map_part(&part_raw, TimelineRole::Assistant, &asid).expect("part should map");
        match part {
            AgentPart::Todo { items } => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].text, "Step 1: check files");
                assert!(items[0].done);
                assert_eq!(items[1].text, "Step 2: edit files");
                assert!(!items[1].done);
                assert_eq!(items[2].text, "Step 3: test changes");
                assert!(!items[2].done);
            }
            _ => panic!("expected AgentPart::Todo"),
        }
    }

    #[test]
    fn test_map_session_tokens_and_limit() {
        let raw = json!({
            "id": "ses-detail",
            "title": "Token test",
            "cost": 0.0,
            "tokens": {
                "input": 5000,
                "output": 200,
                "reasoning": 50,
                "cache": {
                    "read": 12000,
                    "write": 100
                }
            },
            "limit": {
                "context": 1048576,
                "output": 131072
            }
        });

        let s = map_session(&raw).expect("session maps");
        let t = s.tokens.expect("tokens present");
        assert_eq!(t.input, 5000);
        assert_eq!(t.output, 200);
        assert_eq!(t.reasoning, Some(50));
        assert_eq!(t.cache_read, Some(12000));
        assert_eq!(t.cache_write, Some(100));
        assert_eq!(s.limit.unwrap().get("context").unwrap().as_u64(), Some(1048576));
    }

    #[test]
    fn test_map_models_with_variants_and_cost() {
        let models_raw = vec![
            json!({
                "id": "muse-spark-1.3-contributor-free",
                "name": "Muse Spark 1.3 Free",
                "providerID": "opencode",
                "family": "muse",
                "variants": [
                    { "id": "minimal", "settings": { "reasoningEffort": "minimal" } },
                    { "id": "high", "settings": { "reasoningEffort": "high" } }
                ],
                "cost": [{ "input": 0, "output": 0 }],
                "limit": { "context": 1048576 }
            })
        ];

        let models = map_models(&models_raw);
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "muse-spark-1.3-contributor-free");
        let variants = m.variants.as_ref().expect("variants present");
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].id, "minimal");
        assert_eq!(variants[0].reasoning_effort.as_deref(), Some("minimal"));
        assert_eq!(variants[1].id, "high");
        assert_eq!(variants[1].reasoning_effort.as_deref(), Some("high"));
        assert!(m.cost.is_some());
    }

    #[test]
    fn test_normalize_tool_output_array() {
        let asid = AgentSessionId("ses-tool-norm".to_string());
        let part_raw = json!({
            "type": "tool",
            "name": "shell",
            "id": "call-1",
            "state": {
                "status": "completed",
                "content": [
                    { "type": "text", "text": "file1.txt\nfile2.txt" },
                    { "type": "text", "text": "Command exited with code 0." }
                ]
            }
        });

        let part = map_part(&part_raw, TimelineRole::Assistant, &asid).expect("part mapped");
        match part {
            AgentPart::Tool(call) => {
                assert_eq!(
                    call.output.as_ref().and_then(Value::as_str),
                    Some("file1.txt\nfile2.txt\nCommand exited with code 0.")
                );
            }
            _ => panic!("expected Tool part"),
        }
    }

    #[test]
    fn test_map_project() {
        let raw = json!({
            "id": "21eedff21d25eab141956b28d19d6b6cce57839d",
            "canonical": "/home/ryu/Work/muqun/app",
            "vcs": "git",
            "sandboxes": ["sb-1"]
        });

        let p = map_project(&raw).expect("project maps");
        assert_eq!(p.id, "21eedff21d25eab141956b28d19d6b6cce57839d");
        assert_eq!(p.canonical, "/home/ryu/Work/muqun/app");
        assert_eq!(p.name, "app");
        assert_eq!(p.vcs.as_deref(), Some("git"));
        assert_eq!(p.sandboxes, vec!["sb-1".to_string()]);
    }

    #[test]
    fn test_map_session_project_id() {
        let raw = json!({
            "id": "ses_project_test",
            "projectID": "proj_123",
            "title": "Project Session",
            "time": { "created": 1000 }
        });
        let s = map_session(&raw).expect("session maps");
        assert_eq!(s.project_id.as_deref(), Some("proj_123"));
    }

    #[test]
    fn test_map_messages_attachments() {
        let asid = AgentSessionId("ses_att_test".to_string());
        let messages = vec![json!({
            "id": "msg_user_att",
            "type": "user",
            "text": "Check this screenshot",
            "files": [
                { "uri": "file:///tmp/screen.png", "name": "screen.png" }
            ],
            "time": { "created": 1000 }
        })];

        let timeline = map_messages_to_timeline(&messages, &asid);
        assert_eq!(timeline.len(), 1);
        let item = &timeline[0];
        let atts = item.attachments.as_ref().expect("attachments should be present");
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0], "file:///tmp/screen.png");
    }

    #[test]
    fn test_map_agents_mode_color() {
        let raw = vec![json!({
            "id": "explore",
            "name": "explore",
            "description": "Codebase explorer",
            "mode": "subagent",
            "color": "blue"
        })];
        let agents = map_agents(&raw);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].mode.as_deref(), Some("subagent"));
        assert_eq!(agents[0].color.as_deref(), Some("blue"));
    }
}



#[cfg(test)]
mod catalog_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn providers_carry_activation_and_their_models() {
        let raw_providers = vec![
            json!({ "id": "opencode", "name": "OpenCode Zen", "activation": "auto" }),
            json!({ "id": "anthropic", "name": "Anthropic", "activation": "disabled" }),
        ];
        let models = map_models(&[
            json!({
                "id": "union-alpha", "modelID": "union-alpha", "providerID": "opencode",
                "name": "Union Alpha", "enabled": true, "status": "active",
                "limit": { "context": 200000, "output": 32000 },
                "variants": [ { "id": "default" }, { "id": "thinking" } ]
            }),
            json!({
                "id": "claude-x", "modelID": "claude-x", "providerID": "anthropic",
                "name": "Claude X", "enabled": false, "status": "beta"
            }),
        ]);

        let providers = map_providers(&raw_providers, &models);
        assert_eq!(providers.len(), 2);

        let opencode = providers.iter().find(|p| p.id == "opencode").unwrap();
        assert_eq!(opencode.activation.as_deref(), Some("auto"));
        assert_eq!(opencode.models.len(), 1);
        assert!(opencode.models[0].enabled);
        assert_eq!(opencode.models[0].variants.len(), 2);
        assert_eq!(
            opencode.models[0]
                .limit
                .as_ref()
                .and_then(|l| l.get("context"))
                .and_then(Value::as_u64),
            Some(200000)
        );

        let anthropic = providers.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!(anthropic.activation.as_deref(), Some("disabled"));
        assert!(
            !anthropic.models[0].enabled,
            "a disabled model is carried through, not hidden"
        );
    }

    #[test]
    fn a_model_from_an_unlisted_provider_still_appears() {
        let models = map_models(&[json!({
            "id": "m", "modelID": "m", "providerID": "custom", "name": "M"
        })]);
        let providers = map_providers(&[], &models);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "custom");
    }

    #[test]
    fn commands_map_name_and_description() {
        let commands = map_commands(&[
            json!({ "name": "init", "description": "guided AGENTS.md setup" }),
            json!({ "name": "review" }),
            json!({ "description": "no name, skipped" }),
        ]);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name, "init");
        assert_eq!(commands[0].description.as_deref(), Some("guided AGENTS.md setup"));
        assert!(commands[1].description.is_none());
    }

    #[test]
    fn defaults_come_from_model_default_then_config() {
        let default_model = json!({
            "id": "union-alpha", "modelID": "union-alpha", "providerID": "opencode"
        });
        let config = vec![json!({ "config": { "default_agent": "plan" } })];
        let defaults = map_catalog_defaults(Some(&default_model), &config);
        assert_eq!(defaults.agent.as_deref(), Some("plan"));
        let model = defaults.model.expect("a default model");
        assert_eq!(model.provider_id, "opencode");
        assert_eq!(model.model_id, "union-alpha");
    }

    #[test]
    fn a_config_model_string_is_parsed_into_a_model_ref() {
        let config = vec![json!({ "config": { "model": "opencode/glm-5.3-flash#thinking" } })];
        let defaults = map_catalog_defaults(None, &config);
        let model = defaults.model.expect("a default model");
        assert_eq!(model.provider_id, "opencode");
        assert_eq!(model.model_id, "glm-5.3-flash");
        assert_eq!(model.variant.as_deref(), Some("thinking"));
    }

    #[test]
    fn defaults_are_empty_when_opencode_says_nothing() {
        let defaults = map_catalog_defaults(None, &[]);
        assert!(defaults.model.is_none());
        assert!(defaults.agent.is_none());
    }

    #[test]
    fn a_permission_request_carries_save_patterns_and_its_tool_call() {
        let asid = AgentSessionId("ses-1".to_string());
        let raw = json!({
            "id": "per_1",
            "sessionID": "ses-1",
            "action": "external_directory",
            "resources": ["/etc/hosts"],
            "save": ["/etc/*"],
            "source": { "type": "tool", "messageID": "msg_1", "id": "call_1" },
            "metadata": { "why": "probe" }
        });
        let req = map_permission_request(&raw, &asid).expect("maps");
        assert_eq!(req.save, vec!["/etc/*".to_string()]);
        assert_eq!(req.source_message_id.as_deref(), Some("msg_1"));
        assert_eq!(req.source_tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(req.options.len(), 3);
    }

    #[test]
    fn a_form_field_carries_its_conditions_and_constraints() {
        let asid = AgentSessionId("ses-1".to_string());
        let raw = json!({
            "id": "frm_1",
            "sessionID": "ses-1",
            "title": "Deploy",
            "fields": [{
                "key": "tag", "title": "Tag", "type": "string", "required": true,
                "when": [{ "key": "env", "op": "eq", "value": "prod" }],
                "format": "uri", "minLength": 2, "maxLength": 40, "pattern": "^v",
                "custom": true
            }]
        });
        let form = map_form_request(&raw, &asid).expect("maps");
        match &form.fields[0] {
            FormField::String {
                when,
                format,
                min_length,
                max_length,
                pattern,
                custom,
                ..
            } => {
                assert_eq!(when.len(), 1);
                assert_eq!(when[0].key, "env");
                assert_eq!(when[0].op, "eq");
                assert_eq!(format.as_deref(), Some("uri"));
                assert_eq!(*min_length, Some(2));
                assert_eq!(*max_length, Some(40));
                assert_eq!(pattern.as_deref(), Some("^v"));
                assert!(*custom);
            }
            other => panic!("expected a string field, got {other:?}"),
        }
    }

    #[test]
    fn a_compaction_message_becomes_a_boundary_row() {
        let asid = AgentSessionId("ses-1".to_string());
        let msg = json!({
            "id": "msg_c", "type": "compaction", "status": "completed", "reason": "manual",
            "summary": "the summary", "recent": "the tail",
            "time": { "created": 10 },
            "tokens": { "input": 5, "output": 6, "reasoning": 0, "cache": { "read": 0, "write": 0 } },
            "cost": 0.25
        });
        let items = map_message(&msg, &asid);
        assert_eq!(items.len(), 1);
        match &items[0].part {
            AgentPart::Compaction {
                status,
                reason,
                summary,
                recent,
                tokens,
                cost,
                ..
            } => {
                assert_eq!(*status, CompactionStatus::Completed);
                assert_eq!(reason.as_deref(), Some("manual"));
                assert_eq!(summary.as_deref(), Some("the summary"));
                assert_eq!(recent.as_deref(), Some("the tail"));
                assert_eq!(tokens.as_ref().map(|t| t.input), Some(5));
                assert_eq!(*cost, Some(0.25));
            }
            other => panic!("expected a compaction part, got {other:?}"),
        }
    }

    #[test]
    fn a_subagent_tool_is_a_tool_row_not_a_todo_list() {
        let asid = AgentSessionId("ses-1".to_string());
        let part = map_part(
            &json!({
                "type": "tool", "id": "call_1", "name": "subagent",
                "state": {
                    "status": "completed",
                    "input": { "agent": "explore", "description": "count files" },
                    "content": [{ "type": "text", "text": "2" }],
                    "metadata": { "sessionID": "ses_child", "status": "completed" }
                }
            }),
            TimelineRole::Assistant,
            &asid,
        )
        .expect("maps");
        match part {
            AgentPart::Tool(call) => {
                assert_eq!(call.name, "subagent");
                assert_eq!(call.child_session_id.as_deref(), Some("ses_child"));
                assert_eq!(call.title.as_deref(), Some("explore: count files"));
            }
            other => panic!("expected a tool part, got {other:?}"),
        }
    }

    #[test]
    fn a_shell_message_becomes_a_shell_row() {
        let asid = AgentSessionId("ses-1".to_string());
        let items = map_message(
            &json!({
                "id": "msg_s", "type": "shell", "shellID": "sh_1", "command": "sleep 60",
                "status": "running", "time": { "created": 1 }
            }),
            &asid,
        );
        assert_eq!(items.len(), 1);
        match &items[0].part {
            AgentPart::Shell { shell_id, command, status, .. } => {
                assert_eq!(shell_id, "sh_1");
                assert_eq!(command, "sleep 60");
                assert_eq!(status, "running");
            }
            other => panic!("expected a shell part, got {other:?}"),
        }
    }
}
