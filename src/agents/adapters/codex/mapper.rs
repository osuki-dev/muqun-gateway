use std::collections::HashMap;

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
            let text = super::async_questions::display_reply(&text).unwrap_or(text);
            let mut parts = vec![(text_item_id(&message, 0), AgentPart::Text { text })];
            for (index, input) in value["content"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if matches!(input["type"].as_str(), Some("audio" | "localAudio")) {
                    parts.push((
                        part_item_id(&message, index as u64 + 1),
                        AgentPart::Status {
                            text: "Audio attachment (playback unavailable)".into(),
                        },
                    ));
                }
            }
            parts
        }
        "agentMessage" => {
            let mut text = value["text"].as_str().unwrap_or_default().to_owned();
            // Questions remain transcript content without a live control owner.
            // Only native events can create actionable forms in the manager.
            let mut questions = String::new();
            for question in value["questions"].as_array().into_iter().flatten() {
                if let Some(title) = question["title"].as_str() {
                    if !questions.is_empty() {
                        questions.push_str("\n\n");
                    }
                    questions.push_str(title);
                    for option in question["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        questions.push_str("\n- ");
                        questions.push_str(option);
                    }
                }
            }
            // Codex may already project the structured questions into this item's
            // text. Suppress only the exact complete block at paragraph boundaries;
            // unrelated prose and partial question text must still retain questions.
            let already_present = !questions.is_empty()
                && text.match_indices(&questions).any(|(start, block)| {
                    let end = start + block.len();
                    (start == 0 || text[..start].ends_with("\n\n"))
                        && (end == text.len() || text[end..].starts_with("\n\n"))
                });
            if !questions.is_empty() && !already_present {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&questions);
            }
            vec![(text_item_id(&message, 0), AgentPart::Text { text })]
        }
        "hookPrompt" => {
            let text = value["fragments"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|fragment| fragment["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            vec![(
                part_item_id(&message, 0),
                AgentPart::Synthetic {
                    text,
                    description: Some("Hook prompt".into()),
                },
            )]
        }
        "plan" => vec![(
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
        .map(|(ordinal, (id, mut part))| {
            if value["type"] == "imageGeneration" {
                if let (AgentPart::Tool(call), Some(encoded)) =
                    (&mut part, value["result"].as_str())
                {
                    // A saved-path projection has no inline content. Rescue
                    // only PNG-looking generated results, not unavailable text
                    // or arbitrary base64 (which generic originals may serve as
                    // non-image files). Check a fixed prefix here; blocking
                    // egress still validates the complete encoding. Do not apply
                    // the terminal inline size limit to legitimate originals.
                    use base64::Engine as _;
                    let png = encoded
                        .get(..12)
                        .and_then(|head| {
                            base64::engine::general_purpose::STANDARD.decode(head).ok()
                        })
                        .is_some_and(|head| head.starts_with(b"\x89PNG\r\n\x1a\n"));
                    if call.content.is_some() && png {
                        let mut budget =
                            crate::agents::attachment_images::MAX_REFERENCE_RESPONSE_BYTES;
                        if let Some(uri) = crate::agents::attachment_images::inline_reference(
                            "image/png",
                            encoded,
                            &mut budget,
                        ) {
                            call.content = Some(serde_json::json!([
                                {"type":"file", "uri":uri, "mime":"image/png"}
                            ]));
                        }
                    }
                }
            }
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
                generation: None,
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
            generation: None,
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
        nonblocking: false,
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

/// The only catalog command backed by a stable app-server operation.
pub fn is_compact_command(name: &str) -> bool {
    name.trim()
        .strip_prefix('/')
        .unwrap_or(name.trim())
        .eq_ignore_ascii_case("compact")
}

/// Downloaded TUI command rows are descriptions, not executable RPCs.
/// Expose only native compaction, which accepts no prompt or arguments.
pub fn commands_from_catalog(
    commands: &[crate::command_catalog::CatalogCommand],
) -> Vec<CommandInfo> {
    commands
        .iter()
        .filter(|command| is_compact_command(&command.name))
        .map(|command| CommandInfo {
            name: command.name.clone(),
            description: Some(command.description.clone()),
            mode: None,
            template: None,
        })
        .collect()
}

/// Map a Codex `skills/list` result into catalog skills.
///
/// `Ok` means the host provided the list, even when it is empty. `Err` means
/// the payload is not that list — a model page and a slash-command catalog
/// both fail here, so neither is turned into skills. `slash` is true only when
/// the user can invoke the skill by name (`enabled`); otherwise it is false.
pub fn map_skills(response: &Value) -> Result<Vec<SkillInfo>, AgentError> {
    let groups = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Protocol("invalid Codex skills list".into()))?;
    let mut skills: Vec<SkillInfo> = Vec::new();
    let mut index_by_id: HashMap<String, usize> = HashMap::new();
    for group in groups {
        let rows = group
            .get("skills")
            .and_then(Value::as_array)
            .ok_or_else(|| AgentError::Protocol("invalid Codex skills list".into()))?;
        for row in rows {
            let Some(skill) = skill_from_metadata(row) else {
                continue;
            };
            let key = skill.id.to_ascii_lowercase();
            if let Some(&existing) = index_by_id.get(&key) {
                // A skill enabled in one directory wins over a disabled copy.
                if !skills[existing].slash && skill.slash {
                    skills[existing] = skill;
                }
                continue;
            }
            index_by_id.insert(key, skills.len());
            skills.push(skill);
        }
    }
    Ok(skills)
}

fn skill_from_metadata(row: &Value) -> Option<SkillInfo> {
    let id = row
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())?;
    let enabled = row.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    let name = row
        .pointer("/interface/displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(id);
    let description = row
        .pointer("/interface/shortDescription")
        .and_then(Value::as_str)
        .or_else(|| row.get("shortDescription").and_then(Value::as_str))
        .or_else(|| row.get("description").and_then(Value::as_str))
        .unwrap_or("");
    Some(SkillInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        slash: enabled,
        autoinvoke: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{commands_from_catalog, map_skills};
    use crate::command_catalog::CatalogCommand;
    use serde_json::json;

    fn question_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("fixtures/async-question.json")).unwrap()
    }

    fn mapped_question_text(value: &serde_json::Value) -> String {
        let rows = super::item("synthetic-thread", "synthetic-turn", value, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].message_id,
            super::message_id(
                "synthetic-thread",
                "synthetic-turn",
                "synthetic-async-question"
            )
        );
        assert_eq!(rows[0].id, super::text_item_id(&rows[0].message_id, 0));
        let crate::agents::domain::AgentPart::Text { text } = &rows[0].part else {
            panic!("expected transcript text");
        };
        text.clone()
    }

    #[test]
    fn async_question_projection_preserves_already_rendered_complete_block() {
        let mut value = question_fixture();
        let block = value["text"].as_str().unwrap().to_owned();
        for text in [
            block.clone(),
            format!("Please choose.\n\n{block}"),
            format!("{block}\n\nI will wait."),
        ] {
            value["text"] = json!(text);
            assert_eq!(mapped_question_text(&value), text);
        }
    }

    #[test]
    fn async_question_projection_keeps_questions_missing_from_native_text() {
        let mut value = question_fixture();
        let block = value["text"].as_str().unwrap().to_owned();
        for text in [
            String::new(),
            "Please choose.".into(),
            "Which garden should we visit?".into(),
            format!("Quoted: {block}"),
            format!("{block} is the proposal."),
        ] {
            value["text"] = json!(text);
            let expected = if text.is_empty() {
                block.clone()
            } else {
                format!("{text}\n\n{block}")
            };
            assert_eq!(mapped_question_text(&value), expected);
        }
    }

    #[test]
    fn async_question_projection_matches_all_questions_as_one_block() {
        let mut value = question_fixture();
        let first = value["text"].as_str().unwrap().to_owned();
        value["questions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"title":"When should we leave?","options":["Morning","Afternoon"]}));
        let block = format!("{first}\n\nWhen should we leave?\n- Morning\n- Afternoon");
        assert_eq!(mapped_question_text(&value), format!("{first}\n\n{block}"));
        value["text"] = json!(block);
        assert_eq!(mapped_question_text(&value), block);
    }

    #[test]
    fn async_question_projection_does_not_deduplicate_native_text() {
        let mut value = question_fixture();
        let block = value["text"].as_str().unwrap().to_owned();
        let repeated = format!("{block}\n\n{block}");
        value["text"] = json!(repeated);
        assert_eq!(mapped_question_text(&value), repeated);
        value["questions"] = json!([]);
        assert_eq!(mapped_question_text(&value), repeated);
        value["text"] = json!(block);
        let first = super::item("thread", "turn", &value, 0);
        value["id"] = json!("another-item");
        let second = super::item("thread", "turn", &value, 0);
        assert_ne!(first[0].id, second[0].id);
        assert_eq!(first[0].part, second[0].part);
    }

    #[test]
    fn command_catalog_rows_keep_their_names_and_descriptions() {
        let rows = commands_from_catalog(&[CatalogCommand {
            name: "/compact".into(),
            description: "Summarize the conversation".into(),
            args_hint: None,
        }]);
        assert_eq!(rows[0].name, "/compact");
        assert_eq!(
            rows[0].description.as_deref(),
            Some("Summarize the conversation")
        );
        assert!(rows[0].template.is_none());
    }

    #[test]
    fn command_catalog_excludes_tui_and_prompt_only_commands() {
        let commands: Vec<_> = ["/new", "/export", "/compact", "/prompts:custom", "/commit"]
            .into_iter()
            .map(|name| CatalogCommand {
                name: name.into(),
                description: "Synthetic command".into(),
                args_hint: Some("instructions".into()),
            })
            .collect();
        let rows = commands_from_catalog(&commands);
        assert_eq!(rows.len(), 1, "only native compaction is implemented");
        assert_eq!(rows[0].name, "/compact");
        assert!(
            rows[0].template.is_none(),
            "native compact takes no arguments"
        );
        assert!(commands_from_catalog(&commands[..2]).is_empty());
        assert!(commands_from_catalog(&[]).is_empty());
    }

    #[test]
    fn skills_list_maps_name_and_marks_only_enabled_skills_as_invocable() {
        let skills = map_skills(&json!({
            "data": [{
                "cwd": "/synthetic",
                "errors": [],
                "skills": [
                    {
                        "name": "commit",
                        "description": "Write a commit message",
                        "interface": {
                            "displayName": "Commit message",
                            "shortDescription": "Draft the message"
                        },
                        "path": "/synthetic/skills/commit/SKILL.md",
                        "scope": "user",
                        "enabled": true
                    },
                    {
                        "name": "pdf",
                        "description": "Read PDFs",
                        "enabled": false,
                        "path": "/synthetic/skills/pdf/SKILL.md",
                        "scope": "user"
                    },
                    {
                        "name": "pdf",
                        "description": "Read PDFs",
                        "enabled": true,
                        "path": "/synthetic/repo/.codex/skills/pdf/SKILL.md",
                        "scope": "repo"
                    }
                ]
            }]
        }))
        .expect("a skills list");
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].id, "commit");
        assert_eq!(skills[0].name, "Commit message");
        assert_eq!(skills[0].description, "Draft the message");
        assert!(skills[0].slash);
        assert!(!skills[0].autoinvoke);
        // The disabled copy is replaced by the enabled one of the same name.
        assert_eq!(skills[1].id, "pdf");
        assert!(skills[1].slash);
        assert_eq!(skills[1].description, "Read PDFs");
    }

    #[test]
    fn an_empty_skills_list_is_provided_and_other_catalogs_are_not_skills() {
        assert!(map_skills(&json!({"data": []})).unwrap().is_empty());
        assert!(
            map_skills(&json!({"commands": [{"name": "/compact", "description": "Summarize"}]}))
                .is_err(),
            "a slash-command catalog is commands, not skills"
        );
        assert!(
            map_skills(&json!({"data": [{"model": "fixture-model", "displayName": "Fixture"}]}))
                .is_err(),
            "a model page is not a skills list"
        );
    }
}
