use serde_json::Value;

use crate::agents::domain::*;
use crate::agents::ports::agent::AgentError;

pub fn string(value: &Value, key: &str) -> Result<String, AgentError> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AgentError::Protocol(format!("Codex response missing {key}")))
}

pub fn status(value: &Value) -> AgentSessionStatus {
    match value.as_str().or_else(|| value["type"].as_str()) {
        Some("active" | "inProgress") => AgentSessionStatus::Busy,
        Some("idle" | "completed") => AgentSessionStatus::Idle,
        Some("failed" | "systemError") => AgentSessionStatus::Failed,
        Some("interrupted") => AgentSessionStatus::Interrupted,
        _ => AgentSessionStatus::Unknown,
    }
}

pub fn session(thread: &Value) -> Result<AgentSessionInfo, AgentError> {
    let id = string(thread, "id")?;
    let model = if let Some(model) = thread["model"].as_str() {
        Some(ModelRef {
            provider_id: string(thread, "modelProvider")?,
            model_id: model.into(),
            variant: thread["reasoningEffort"].as_str().map(str::to_owned),
        })
    } else {
        None
    };
    let fork = thread["forkedFromId"]
        .as_str()
        .map(|parent| SessionForkInfo {
            session_id: parent.into(),
            boundary_type: None,
            message_id: None,
        });
    Ok(AgentSessionInfo {
        asid: AgentSessionId(id),
        agent_id: String::new(),
        backend_session_id: String::new(),
        runtime: None,
        context_usage: None,
        git_info: thread["gitInfo"].as_object().map(|git| SessionGitInfo {
            sha: git.get("sha").and_then(Value::as_str).map(str::to_owned),
            branch: git.get("branch").and_then(Value::as_str).map(str::to_owned),
            origin_url: git
                .get("originUrl")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        agent_name: thread["agentNickname"].as_str().map(str::to_owned),
        agent_role: thread["agentRole"].as_str().map(str::to_owned),
        title: thread["name"]
            .as_str()
            .or_else(|| thread["preview"].as_str())
            .unwrap_or("")
            .into(),
        mode: None,
        model,
        status: status(&thread["status"]),
        directory: thread["cwd"].as_str().map(str::to_owned),
        cost: None,
        tokens: None,
        limit: None,
        parent_id: thread["parentThreadId"].as_str().map(str::to_owned),
        project_id: thread["projectId"].as_str().map(str::to_owned),
        outcome: None,
        error: None,
        revert: None,
        fork,
        time_idle: None,
        time_viewed: None,
        deleted: false,
        updated_ms: thread["updatedAt"]
            .as_u64()
            .unwrap_or(0)
            .saturating_mul(1000),
    })
}

/// Raw producer identities, independent of presentation order. The thread is
/// part of each row id, preventing collisions across projections.
pub fn message_id(thread: &str, turn: &str, item: &str) -> String {
    format!("{thread}/{turn}/{item}")
}

pub fn order_group(thread: &str, turn: &str, position: u64) -> String {
    format!("{thread}/{turn}/{position:020}")
}

pub fn item(thread: &str, turn: &str, value: &Value, at_ms: u64) -> Vec<TimelineItem> {
    let Some(id) = value["id"].as_str() else {
        return vec![];
    };
    let message = message_id(thread, turn, id);
    let mut role = TimelineRole::Assistant;
    let attachments = (value["type"] == "userMessage")
        .then(|| {
            value["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|input| match input["type"].as_str()? {
                    "image" => input["url"].as_str(),
                    "localImage" => input["path"].as_str(),
                    _ => None,
                })
                .filter(|source| !source.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|sources| !sources.is_empty());
    let parts = match value["type"].as_str().unwrap_or("") {
        "userMessage" => {
            role = TimelineRole::User;
            let text = value["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            vec![(text_item_id(&message, 0), AgentPart::Text { text })]
        }
        "agentMessage" | "plan" => vec![(
            text_item_id(&message, 0),
            AgentPart::Text {
                text: value["text"].as_str().unwrap_or("").into(),
            },
        )],
        "reasoning" => {
            // Summary and raw reasoning have different indexed delta streams.
            let mut parts = vec![];
            for (key, offset) in [("summary", 0), ("content", 1_000_000)] {
                if let Some(texts) = value[key].as_array() {
                    for (index, text) in texts.iter().enumerate() {
                        if let Some(text) = text.as_str() {
                            parts.push((
                                reasoning_item_id(&message, offset + index as u64),
                                AgentPart::Reasoning {
                                    text: text.into(),
                                    duration_ms: None,
                                },
                            ));
                        }
                    }
                }
            }
            parts
        }
        _ => super::items::parts(&message, id, value),
    };
    parts
        .into_iter()
        .enumerate()
        .map(|(ordinal, (id, part))| {
            let ordinal = if matches!(part, AgentPart::Reasoning { .. }) {
                id.rsplit_once(":r")
                    .and_then(|(_, n)| n.parse().ok())
                    .unwrap_or(ordinal as u64)
            } else {
                ordinal as u64
            };
            TimelineItem {
                id,
                message_id: message.clone(),
                role,
                part,
                seq: 0,
                updated_ms: at_ms,
                ordinal,
                attachments: (ordinal == 0).then(|| attachments.clone()).flatten(),
                image_assets: None,
            }
        })
        .collect()
}

/// Turn-level updates have their own event identity; they are not file items.
pub fn turn_update(thread: &str, turn: &str, method: &str, params: &Value) -> Vec<TimelineItem> {
    let parts = match method {
        "turn/plan/updated" => {
            let Some(plan) = params["plan"].as_array() else {
                return vec![];
            };
            let mut parts = vec![AgentPart::Todo {
                items: plan
                    .iter()
                    .filter_map(|step| {
                        Some(TodoItem {
                            text: step["step"].as_str()?.into(),
                            done: step["status"] == "completed",
                        })
                    })
                    .collect(),
            }];
            if let Some(text) = params["explanation"].as_str().filter(|s| !s.is_empty()) {
                parts.push(AgentPart::Text { text: text.into() });
            }
            parts
        }
        "turn/diff/updated" => {
            let Some(diff) = params["diff"].as_str() else {
                return vec![];
            };
            vec![AgentPart::Diff {
                file: String::new(),
                diff: diff.into(),
            }]
        }
        _ => return vec![],
    };
    parts
        .into_iter()
        .enumerate()
        .map(|(ordinal, part)| TimelineItem {
            id: format!("{thread}/{turn}:event:{method}:{ordinal}"),
            message_id: format!("{thread}/{turn}:event:{method}"),
            role: TimelineRole::Assistant,
            part,
            seq: 0,
            updated_ms: 0,
            ordinal: ordinal as u64,
            attachments: None,
            image_assets: None,
        })
        .collect()
}

pub fn permission(id: &Value, method: &str, params: &Value) -> Option<PermissionRequest> {
    if !matches!(
        method,
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    ) {
        return None;
    }
    if method == "item/fileChange/requestApproval" && !params["grantRoot"].is_null() {
        // This unstable field asks for broader session writes, not the
        // one-action Allow semantics the neutral App can express.
        return None;
    }
    let thread = params["threadId"].as_str()?;
    let turn = params["turnId"].as_str()?;
    let item = params["itemId"].as_str()?;
    let action = match method {
        "item/commandExecution/requestApproval" => "shell",
        "item/fileChange/requestApproval" => "edit",
        _ => "permissions",
    };
    Some(PermissionRequest {
        id: super::client::request_key(id)?,
        asid: AgentSessionId(thread.into()),
        action: action.into(),
        resources: params["command"]
            .as_str()
            .into_iter()
            .map(str::to_owned)
            .collect(),
        save: vec![],
        prompt: params["reason"]
            .as_str()
            .unwrap_or("Codex requests approval")
            .into(),
        tool: Some(action.into()),
        source_message_id: Some(message_id(thread, turn, item)),
        source_tool_call_id: Some(item.into()),
        metadata: Some(params.clone()),
        message: None,
        // Session grants and policy amendments are deliberately not mapped to
        // permanent AllowAlways. Only the exact one-action grant is offered.
        options: vec![
            PermissionOption {
                index: 0,
                label: if action == "permissions" {
                    "Allow for this turn"
                } else {
                    "Allow once"
                }
                .into(),
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

pub fn form(id: &Value, method: &str, params: &Value) -> Option<FormRequest> {
    if method != "item/tool/requestUserInput" {
        return None;
    }
    let questions = params["questions"].as_array()?;
    if questions.is_empty() || questions.len() > 32 {
        return None;
    }
    let mut fields = vec![];
    for question in questions {
        // The existing App form has no secret-input semantics.
        if question["isSecret"].as_bool()? {
            return None;
        }
        let key = question["id"].as_str()?.to_string();
        let options = match question.get("options")? {
            Value::Null => vec![],
            Value::Array(a) => a
                .iter()
                .map(|v| {
                    let label = v["label"].as_str()?;
                    Some(FormOption {
                        value: label.into(),
                        label: label.into(),
                        description: v["description"].as_str().map(str::to_owned),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            _ => return None,
        };
        fields.push(FormField::String {
            key,
            title: question["question"].as_str()?.into(),
            description: question["header"].as_str().map(str::to_owned),
            required: true,
            when: vec![],
            placeholder: None,
            default: None,
            options,
            format: None,
            min_length: None,
            max_length: None,
            pattern: None,
            custom: question["isOther"].as_bool()?,
        });
    }
    Some(FormRequest {
        id: super::client::request_key(id)?,
        asid: AgentSessionId(params["threadId"].as_str()?.into()),
        title: "Codex question".into(),
        fields,
    })
}

pub fn catalog(response: &Value, provider: &str) -> Result<AgentCatalog, AgentError> {
    let rows = response["data"]
        .as_array()
        .ok_or_else(|| AgentError::Protocol("invalid Codex model catalog".into()))?;
    let mut catalog = AgentCatalog::default();
    for row in rows {
        if row["hidden"] == true {
            continue;
        }
        let id = string(row, "model")?;
        let variants: Vec<_> = row["supportedReasoningEfforts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["reasoningEffort"].as_str())
            .map(|v| ModelVariantInfo {
                id: v.into(),
                reasoning_effort: Some(v.into()),
            })
            .collect();
        catalog.models.push(ModelInfo {
            id: id.clone(),
            name: string(row, "displayName")?,
            provider_id: provider.into(),
            family: None,
            limit: None,
            variants: Some(variants),
            cost: None,
            enabled: true,
            status: None,
        });
        if row["isDefault"] == true {
            catalog.defaults.model = Some(ModelRef {
                provider_id: provider.into(),
                model_id: id,
                variant: row["defaultReasoningEffort"].as_str().map(str::to_owned),
            });
        }
    }
    Ok(catalog)
}
