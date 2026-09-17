use serde_json::Value;
use crate::agent::domain::{
    AgentInfo, AgentPart, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus,
    FormField, FormOption, FormRequest, McpServerInfo, ModelInfo, ModelRef,
    ModelVariantInfo, PermissionDecision, PermissionOption, PermissionRequest, SkillInfo, TimelineItem,
    TimelineRole, TodoItem, TokensUsage, ToolCallStatus,
};

pub fn map_session(val: &Value) -> Option<AgentSessionInfo> {
    let item = val.get("data").unwrap_or(val);
    let id = item.get("id").and_then(Value::as_str)?;
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();

    let agent = item.get("agent").and_then(Value::as_str).map(str::to_string);

    let model = item
        .get("model")
        .and_then(|m| {
            let model_id = m.get("id").or_else(|| m.get("modelID")).and_then(Value::as_str)?;
            let provider_id = m.get("providerID").and_then(Value::as_str).unwrap_or("opencode");
            let variant = m.get("variant").and_then(Value::as_str).map(str::to_string);
            Some(ModelRef {
                provider_id: provider_id.to_string(),
                model_id: model_id.to_string(),
                variant,
            })
        })
        .or_else(|| {
            Some(ModelRef {
                provider_id: "opencode".to_string(),
                model_id: "big-pickle".to_string(),
                variant: None,
            })
        });

    let cost = item.get("cost").and_then(Value::as_f64);
    let tokens = item.get("tokens").and_then(|t| {
        Some(TokensUsage {
            input: t.get("input").and_then(Value::as_u64).unwrap_or(0),
            output: t.get("output").and_then(Value::as_u64).unwrap_or(0),
            reasoning: t.get("reasoning").and_then(Value::as_u64),
            cache_read: t.pointer("/cache/read").and_then(Value::as_u64),
            cache_write: t.pointer("/cache/write").and_then(Value::as_u64),
        })
    });
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

    Some(AgentSessionInfo {
        asid: AgentSessionId(id.to_string()),
        backend_session_id: id.to_string(),
        title,
        agent,
        model,
        status: AgentSessionStatus::Idle,
        directory,
        cost,
        tokens,
        limit,
        parent_id,
        project_id,
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

fn is_duplicate_tool_text(text: &str, prev_item: Option<&TimelineItem>) -> bool {
    let Some(prev) = prev_item else {
        return false;
    };
    if let AgentPart::Tool { ref output, .. } = prev.part {
        let clean_text = text
            .trim()
            .trim_start_matches("```")
            .trim_end_matches("```")
            .replace("Command exited with code 0.", "")
            .replace("Command exited with code 0", "")
            .trim()
            .to_string();

        let tool_out_str = match output {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        };
        let clean_tool = tool_out_str
            .replace("Command exited with code 0.", "")
            .replace("Command exited with code 0", "")
            .trim()
            .to_string();

        if clean_text.is_empty()
            || (!clean_tool.is_empty()
                && (clean_text == clean_tool
                    || clean_tool.contains(&clean_text)
                    || clean_text.contains(&clean_tool)))
        {
            return true;
        }
    }
    false
}

pub fn map_messages_to_timeline(messages: &[Value], asid: &AgentSessionId) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut seq = 1;

    let mut ordered: Vec<&Value> = messages.iter().collect();
    if ordered.len() > 1 {
        let first_t = ordered.first().and_then(|m| m.pointer("/time/created")).and_then(Value::as_u64).unwrap_or(0);
        let last_t = ordered.last().and_then(|m| m.pointer("/time/created")).and_then(Value::as_u64).unwrap_or(0);
        if first_t > last_t {
            ordered.reverse();
        }
    }

    for msg in ordered {
        let msg_id = msg.get("id").and_then(Value::as_str).unwrap_or("unknown");
        let role = match msg.get("type").and_then(Value::as_str) {
            Some("user") => TimelineRole::User,
            Some("system") => TimelineRole::System,
            _ => TimelineRole::Assistant,
        };

        let updated_ms = msg
            .pointer("/time/completed")
            .or_else(|| msg.pointer("/time/created"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let attachments: Option<Vec<String>> = msg
            .get("files")
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
            .filter(|v: &Vec<String>| !v.is_empty());

        // Check top-level "text" (standard for user messages)
        if let Some(text) = msg.get("text").and_then(Value::as_str) {
            if !text.trim().is_empty() {
                if !is_duplicate_tool_text(text, items.last()) {
                    let item_id = format!("{msg_id}:0");
                    items.push(TimelineItem {
                        id: item_id,
                        message_id: msg_id.to_string(),
                        role,
                        part: AgentPart::Text {
                            text: text.to_string(),
                        },
                        seq,
                        updated_ms,
                        attachments: attachments.clone(),
                    });
                    seq += 1;
                }
            }
        }

        // In OpenCode V2, parts are in `content` or `parts`
        let content_parts = msg
            .get("content")
            .or_else(|| msg.get("parts"))
            .and_then(Value::as_array);

        if let Some(parts) = content_parts {
            for (idx, part) in parts.iter().enumerate() {
                if let Some(agent_part) = map_part(part, role, asid) {
                    if let AgentPart::Text { ref text } = &agent_part {
                        if is_duplicate_tool_text(text, items.last()) {
                            continue;
                        }
                    }

                    if let AgentPart::Reasoning { .. } = &agent_part {
                        if let Some(last_item) = items.last_mut() {
                            if last_item.message_id == msg_id {
                                if let AgentPart::Reasoning { .. } = &last_item.part {
                                    last_item.part = agent_part;
                                    last_item.updated_ms = updated_ms;
                                    continue;
                                }
                            }
                        }
                    }

                    let item_id = format!("{msg_id}:{idx}");
                    items.push(TimelineItem {
                        id: item_id,
                        message_id: msg_id.to_string(),
                        role,
                        part: agent_part,
                        seq,
                        updated_ms,
                        attachments: if idx == 0 { attachments.clone() } else { None },
                    });
                    seq += 1;
                }
            }
        }
    }

    items
}

fn extract_todo_items(val: &Value) -> Option<Vec<TodoItem>> {
    let raw_items = val
        .get("items")
        .or_else(|| val.pointer("/state/input/todos"))
        .or_else(|| val.pointer("/input/todos"))
        .or_else(|| val.pointer("/state/metadata/todos"))
        .or_else(|| val.get("todos"));

    if let Some(arr) = raw_items.and_then(Value::as_array) {
        let items: Vec<TodoItem> = arr
            .iter()
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
            .collect();
        if !items.is_empty() {
            return Some(items);
        }
    }

    let output = val.pointer("/state/output").or_else(|| val.get("output"));
    if let Some(arr) = output.and_then(Value::as_array) {
        let items: Vec<TodoItem> = arr
            .iter()
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
            .collect();
        if !items.is_empty() {
            return Some(items);
        }
    } else if let Some(s) = output.and_then(Value::as_str) {
        if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(s) {
            let items: Vec<TodoItem> = arr
                .iter()
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
                .collect();
            if !items.is_empty() {
                return Some(items);
            }
        }
    }

    None
}

fn normalize_tool_output(output: Option<Value>) -> Option<Value> {
    let val = output?;
    match val {
        Value::Array(arr) => {
            let mut parts = Vec::new();
            let mut all_text = true;
            for item in &arr {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                } else if let Some(s) = item.as_str() {
                    parts.push(s.to_string());
                } else {
                    all_text = false;
                    break;
                }
            }
            if all_text && !parts.is_empty() {
                Some(Value::String(parts.join("\n")))
            } else {
                Some(Value::Array(arr))
            }
        }
        Value::String(s) => {
            if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(&s) {
                normalize_tool_output(Some(Value::Array(arr)))
            } else {
                Some(Value::String(s))
            }
        }
        other => Some(other),
    }
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
            let duration_ms = val.get("durationMs").and_then(Value::as_u64);
            Some(AgentPart::Reasoning {
                text,
                duration_ms,
            })
        }
        "tool" | "tool_use" | "tool_call" | "tool-call" => {
            let id = val.get("id").and_then(Value::as_str).unwrap_or("unknown");
            let name = val
                .get("name")
                .or_else(|| val.get("tool"))
                .and_then(Value::as_str)
                .unwrap_or("tool");

            if name == "todowrite" || name == "todo" || name == "task" || name == "tasks" {
                if let Some(items) = extract_todo_items(val) {
                    return Some(AgentPart::Todo { items });
                }
            }

            let state = val.get("state");
            let status = match state.and_then(|s| s.get("status")).and_then(Value::as_str) {
                Some("completed") => ToolCallStatus::Completed,
                Some("failed") | Some("error") => ToolCallStatus::Failed,
                _ => ToolCallStatus::Running,
            };

            let input = state
                .and_then(|s| s.get("input"))
                .or_else(|| val.get("input"))
                .cloned()
                .unwrap_or(Value::Null);

            let raw_output = state
                .and_then(|s| s.get("output"))
                .or_else(|| val.get("output"))
                .or_else(|| state.and_then(|s| s.get("content")))
                .cloned();
            let output = normalize_tool_output(raw_output);

            Some(AgentPart::Tool {
                id: id.to_string(),
                name: name.to_string(),
                input,
                output,
                status,
            })
        }
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

    let resources: Vec<String> = val
        .get("resources")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let message = val.get("message").and_then(Value::as_str).map(str::to_string);
    let tool = val.get("tool").and_then(Value::as_str).map(str::to_string);

    let prompt = if let Some(ref msg) = message {
        msg.clone()
    } else if !resources.is_empty() {
        format!("{action}: {}", resources.join(", "))
    } else {
        format!("Allow {action}?")
    };

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
        prompt,
        tool,
        message,
        options,
    })
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
                    placeholder,
                    default,
                    options,
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
                    url,
                });
            }
            _ => {
                fields.push(FormField::Unknown {
                    key,
                    title: field_title,
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
            AgentPart::Tool { name, status, input, output, .. } => {
                assert_eq!(name, "bash");
                assert_eq!(*status, ToolCallStatus::Completed);
                assert_eq!(input.get("command").unwrap(), "cargo check");
                assert_eq!(output.as_ref().unwrap().as_str(), Some("Finished dev profile"));
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
    fn test_map_session_model_fallback() {
        let raw_without_model = json!({
            "id": "ses-no-model",
            "title": "Untitled",
            "agent": "build",
        });
        let session = map_session(&raw_without_model).expect("should map");
        let model = session.model.expect("model should be defaulted");
        assert_eq!(model.provider_id, "opencode");
        assert_eq!(model.model_id, "big-pickle");
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
            AgentPart::Tool { output, .. } => {
                assert_eq!(
                    output.as_ref().and_then(Value::as_str),
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


