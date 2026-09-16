use serde_json::Value;
use crate::agent::domain::{
    AgentInfo, AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus,
    FormField, FormOption, FormRequest, McpServerInfo, ModelInfo, ModelRef,
    PermissionDecision, PermissionOption, PermissionRequest, TimelineItem,
    TimelineRole, TodoItem, TokensUsage, ToolCallStatus,
};

pub fn map_session(val: &Value) -> Option<AgentSessionInfo> {
    let id = val.get("id").and_then(Value::as_str)?;
    let title = val
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();

    let agent = val.get("agent").and_then(Value::as_str).map(str::to_string);

    let model = val.get("model").and_then(|m| {
        let model_id = m.get("id").or_else(|| m.get("modelID")).and_then(Value::as_str)?;
        let provider_id = m.get("providerID").and_then(Value::as_str).unwrap_or("unknown");
        let variant = m.get("variant").and_then(Value::as_str).map(str::to_string);
        Some(ModelRef {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            variant,
        })
    });

    let cost = val.get("cost").and_then(Value::as_f64);
    let tokens = val.get("tokens").and_then(|t| {
        Some(TokensUsage {
            input: t.get("input").and_then(Value::as_u64).unwrap_or(0),
            output: t.get("output").and_then(Value::as_u64).unwrap_or(0),
            reasoning: t.get("reasoning").and_then(Value::as_u64),
        })
    });

    let directory = val
        .pointer("/location/directory")
        .or_else(|| val.get("directory"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let updated_ms = val
        .pointer("/time/updated")
        .or_else(|| val.pointer("/time/created"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

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
        updated_ms,
    })
}

pub fn map_messages_to_timeline(messages: &[Value], asid: &AgentSessionId) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut seq = 1;

    for msg in messages {
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

        // In OpenCode V2, parts are in `content` or `parts`
        let content_parts = msg
            .get("content")
            .or_else(|| msg.get("parts"))
            .and_then(Value::as_array);

        if let Some(parts) = content_parts {
            for (idx, part) in parts.iter().enumerate() {
                if let Some(agent_part) = map_part(part, role, asid) {
                    let item_id = format!("{msg_id}:{idx}");
                    items.push(TimelineItem {
                        id: item_id,
                        message_id: msg_id.to_string(),
                        role,
                        part: agent_part,
                        seq,
                        updated_ms,
                    });
                    seq += 1;
                }
            }
        }
    }

    items
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
            let text = val
                .get("text")
                .or_else(|| val.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let duration_ms = val.get("durationMs").and_then(Value::as_u64);
            Some(AgentPart::Reasoning {
                text: text.to_string(),
                duration_ms,
            })
        }
        "tool" | "tool_use" | "tool_call" | "tool-call" => {
            let id = val.get("id").and_then(Value::as_str).unwrap_or("unknown");
            let name = val.get("name").and_then(Value::as_str).unwrap_or("tool");

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

            let output = state
                .and_then(|s| s.get("output"))
                .or_else(|| val.get("output"))
                .cloned();

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
            let raw_items = val.get("items").and_then(Value::as_array)?;
            let items = raw_items
                .iter()
                .filter_map(|it| {
                    let text = it.get("text").and_then(Value::as_str)?.to_string();
                    let done = it.get("done").and_then(Value::as_bool).unwrap_or(false);
                    Some(TodoItem { text, done })
                })
                .collect();
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
            Some(ModelInfo {
                id,
                name,
                provider_id,
                family,
                limit,
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
            Some(AgentInfo {
                id,
                name,
                description,
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
}

