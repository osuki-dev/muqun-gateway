use serde_json::{json, Value};

use crate::agent::domain::{
    AgentCatalog, AgentInfo, AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus,
    CatalogDefaults, CommandInfo, ModelInfo, ModelRef, ModelVariantInfo, ProviderInfo,
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
        backend_session_id: session_id.to_string(),
        title,
        agent: Some("deepseek".to_string()),
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
pub fn map_catalog(raw: &Value) -> AgentCatalog {
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

    // Fallback if empty
    if models.is_empty() {
        let v3 = ModelVariantInfo {
            id: "default".to_string(),
            reasoning_effort: None,
        };
        models.push(ModelInfo {
            id: "deepseek-chat".to_string(),
            name: "DeepSeek-V3".to_string(),
            provider_id: "deepseek".to_string(),
            family: Some("deepseek".to_string()),
            limit: None,
            variants: Some(vec![v3.clone()]),
            cost: None,
            enabled: true,
            status: Some("active".to_string()),
        });
        models.push(ModelInfo {
            id: "deepseek-reasoner".to_string(),
            name: "DeepSeek-R1".to_string(),
            provider_id: "deepseek".to_string(),
            family: Some("deepseek".to_string()),
            limit: None,
            variants: Some(vec![v3.clone()]),
            cost: None,
            enabled: true,
            status: Some("active".to_string()),
        });
        providers.push(ProviderInfo {
            id: "deepseek".to_string(),
            name: "DeepSeek Official".to_string(),
            activation: Some("enabled".to_string()),
            models: vec![
                ProviderModelInfo {
                    id: "deepseek-chat".to_string(),
                    name: "DeepSeek-V3".to_string(),
                    enabled: true,
                    variants: vec![v3.clone()],
                    limit: None,
                    status: Some("active".to_string()),
                },
                ProviderModelInfo {
                    id: "deepseek-reasoner".to_string(),
                    name: "DeepSeek-R1".to_string(),
                    enabled: true,
                    variants: vec![v3],
                    limit: None,
                    status: Some("active".to_string()),
                },
            ],
        });
    }

    let default_agent = AgentInfo {
        id: "general".to_string(),
        name: "general".to_string(),
        description: Some("DeepSeek Harness General Agent".to_string()),
        mode: Some("primary".to_string()),
        color: None,
        hidden: false,
        model: default_model_ref.clone(),
    };

    AgentCatalog {
        models,
        agents: vec![default_agent],
        mcp: vec![],
        skills: vec![SkillInfo {
            id: "deepseek".to_string(),
            name: "deepseek".to_string(),
            description: "DeepSeek Harness Tools".to_string(),
            slash: true,
            autoinvoke: true,
        }],
        providers,
        commands: vec![CommandInfo {
            name: "help".to_string(),
            description: Some("Show available help".to_string()),
            agent: None,
            template: None,
        }],
        defaults: CatalogDefaults {
            model: default_model_ref,
            agent: Some("general".to_string()),
        },
    }
}

/// Map historical records from `session/page` into `Vec<TimelineItem>`.
pub fn map_timeline_records(raw_page: &Value) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let records = match raw_page.get("records").and_then(Value::as_array) {
        Some(r) => r,
        None => return items,
    };

    for entry in records {
        let event = match entry.get("event") {
            Some(e) => e,
            None => continue,
        };

        let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
        let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let time = event.get("time").and_then(Value::as_u64).unwrap_or(0);
        let data = match event.get("data") {
            Some(d) => d,
            None => continue,
        };

        if event_type == "user/message" {
            let role_str = data.get("role").and_then(Value::as_str).unwrap_or("user");
            let role = if role_str == "system" {
                TimelineRole::System
            } else {
                TimelineRole::User
            };
            let msg_id = data
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("msg_{seq}"));

            if let Some(content_parts) = data.get("content").and_then(Value::as_array) {
                for (idx, part) in content_parts.iter().enumerate() {
                    let ptype = part.get("type").and_then(Value::as_str).unwrap_or("text");
                    if ptype == "text" {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        items.push(TimelineItem {
                            id: format!("{msg_id}:p{idx}"),
                            message_id: msg_id.clone(),
                            role,
                            part: AgentPart::Text {
                                text: text.to_string(),
                            },
                            seq,
                            updated_ms: time,
                            ordinal: idx as u64,
                            attachments: None,
                        });
                    }
                }
            }
        } else if event_type == "assistant/message" {
            let role = TimelineRole::Assistant;
            let msg = data.get("message").unwrap_or(data);
            let msg_id = msg
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("msg_{seq}"));

            if let Some(content_parts) = msg.get("content").and_then(Value::as_array) {
                for (idx, part) in content_parts.iter().enumerate() {
                    let ptype = part.get("type").and_then(Value::as_str).unwrap_or("text");
                    if ptype == "reasoning" {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            items.push(TimelineItem {
                                id: format!("{msg_id}:r{idx}"),
                                message_id: msg_id.clone(),
                                role,
                                part: AgentPart::Reasoning {
                                    text: text.to_string(),
                                    duration_ms: None,
                                },
                                seq,
                                updated_ms: time,
                                ordinal: idx as u64,
                                attachments: None,
                            });
                        }
                    } else if ptype == "text" {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        items.push(TimelineItem {
                            id: format!("{msg_id}:t{idx}"),
                            message_id: msg_id.clone(),
                            role,
                            part: AgentPart::Text {
                                text: text.to_string(),
                            },
                            seq,
                            updated_ms: time,
                            ordinal: idx as u64,
                            attachments: None,
                        });
                    } else if ptype == "tool" || ptype == "tool_call" {
                        let tool_id = part
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("call_unknown")
                            .to_string();
                        let tool_name = part
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string();
                        let input = part.get("input").cloned().unwrap_or(Value::Null);
                        let output = part.get("output").cloned();
                        let is_err = part.get("error").is_some();
                        let tool_call =
                            project_tool_call(tool_id, tool_name, input, output, is_err);

                        items.push(TimelineItem {
                            id: format!("{msg_id}:tool{idx}"),
                            message_id: msg_id.clone(),
                            role,
                            part: AgentPart::Tool(tool_call),
                            seq,
                            updated_ms: time,
                            ordinal: idx as u64,
                            attachments: None,
                        });
                    }
                }
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn maps_deepseek_timeline_records() {
        let raw = json!({
            "records": [
                {
                    "type": "event",
                    "event": {
                        "type": "user/message",
                        "seq": 1,
                        "time": 1790558000000u64,
                        "data": {
                            "id": "user_msg_1",
                            "role": "user",
                            "content": [
                                { "type": "text", "text": "Hello DeepSeek" }
                            ]
                        }
                    }
                },
                {
                    "type": "event",
                    "event": {
                        "type": "assistant/message",
                        "seq": 2,
                        "time": 1790558001000u64,
                        "data": {
                            "message": {
                                "id": "asst_msg_1",
                                "role": "assistant",
                                "content": [
                                    { "type": "reasoning", "text": "thinking..." },
                                    { "type": "text", "text": "Hello! How can I help?" }
                                ]
                            }
                        }
                    }
                }
            ]
        });

        let items = map_timeline_records(&raw);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].role, TimelineRole::User);
        assert_eq!(
            items[0].part,
            AgentPart::Text {
                text: "Hello DeepSeek".into()
            }
        );

        assert_eq!(items[1].role, TimelineRole::Assistant);
        assert_eq!(
            items[1].part,
            AgentPart::Reasoning {
                text: "thinking...".into(),
                duration_ms: None
            }
        );

        assert_eq!(items[2].role, TimelineRole::Assistant);
        assert_eq!(
            items[2].part,
            AgentPart::Text {
                text: "Hello! How can I help?".into()
            }
        );
    }
}
