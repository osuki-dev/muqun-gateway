use super::projection::{active_root_run, pending_request, rows, string};
use crate::agents::adapters::t3::{client::ms_from_iso, mapper as common};
use crate::agents::domain::{
    AgentPart, AgentSessionInfo, AgentSessionStatus, FormField, FormOption, FormRequest,
    PermissionDecision, PermissionOption, PermissionRequest, TimelineItem, TimelineRole, TodoItem,
    ToolCall, ToolCallStatus, ToolTime,
};
use crate::agents::ports::agent::AgentError;
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn shell_status(thread: &Value) -> AgentSessionStatus {
    match string(thread, "status") {
        Some("preparing" | "starting" | "running" | "waiting") => AgentSessionStatus::Busy,
        Some("failed") => AgentSessionStatus::Failed,
        Some("interrupted" | "cancelled") => AgentSessionStatus::Interrupted,
        _ => AgentSessionStatus::Idle,
    }
}

pub fn session(
    thread: &Value,
    projection: Option<&Value>,
    roots: &HashMap<String, String>,
) -> Option<AgentSessionInfo> {
    // Shared metadata fields retain the same contract; execution state does not.
    let mut info = common::map_session(thread, roots)?;
    info.status = projection.map_or_else(
        || shell_status(thread),
        |p| {
            if super::projection::has_active_run(p) {
                AgentSessionStatus::Busy
            } else {
                AgentSessionStatus::Idle
            }
        },
    );
    info.parent_id = thread
        .pointer("/lineage/parentThreadId")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(p) = projection {
        let last = rows(p, "runs")
            .iter()
            .max_by_key(|run| run.get("ordinal").and_then(Value::as_u64).unwrap_or(0));
        if !super::projection::has_active_run(p) {
            info.status = last.map(shell_status).unwrap_or(AgentSessionStatus::Idle);
        }
        info.time_idle = last
            .and_then(|run| string(run, "completedAt"))
            .and_then(ms_from_iso);
        info.outcome = last.and_then(|run| outcome(string(run, "status")));
    } else {
        info.outcome = outcome(string(thread, "status"));
    }
    if info.status == AgentSessionStatus::Failed {
        let message = string(thread, "lastError")
            .or_else(|| {
                projection.and_then(|p| {
                    rows(p, "visibleTurnItems").iter().rev().find_map(|row| {
                        row.pointer("/item/failure/message").and_then(Value::as_str)
                    })
                })
            })
            .unwrap_or("the provider reported an error");
        info.error = Some(crate::agents::domain::AgentErrorInfo {
            retry_after_ms: None,
            name: "ProviderError".into(),
            message: crate::agents::adapters::t3::endpoint::short(message),
            status: None,
        });
    }
    Some(info)
}

fn outcome(status: Option<&str>) -> Option<String> {
    match status {
        Some("completed") => Some("succeeded".into()),
        Some("failed") => Some("failed".into()),
        Some("interrupted" | "cancelled") => Some("interrupted".into()),
        _ => None,
    }
}

pub fn shell_sessions(shell: &Value) -> Vec<AgentSessionInfo> {
    let roots = common::project_roots(shell);
    let mut result: Vec<_> = rows(shell, "threads")
        .iter()
        .filter(|thread| {
            thread.get("deletedAt").is_none_or(Value::is_null)
                && thread.get("archivedAt").is_none_or(Value::is_null)
        })
        .filter_map(|thread| session(thread, None, &roots))
        .collect();
    result.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
    result
}

fn decision(raw: &str) -> Option<PermissionDecision> {
    match raw {
        "accept" => Some(PermissionDecision::Allow),
        "acceptAlways" => Some(PermissionDecision::AllowAlways),
        "decline" => Some(PermissionDecision::Deny),
        _ => None,
    }
}

pub fn permission(thread_id: &str, item: &Value) -> Option<PermissionRequest> {
    let options: Vec<_> = rows(item, "options")
        .iter()
        .enumerate()
        .filter_map(|(index, option)| {
            Some(PermissionOption {
                index,
                label: string(option, "label")?.to_string(),
                decision: decision(string(option, "decision")?)?,
            })
        })
        .collect();
    if options.is_empty() {
        return None;
    }
    Some(PermissionRequest {
        id: string(item, "requestId")?.to_string(),
        asid: thread_id.into(),
        action: string(item, "requestKind")
            .unwrap_or("permission")
            .to_string(),
        resources: vec![],
        save: vec![],
        prompt: string(item, "prompt")
            .or_else(|| string(item, "title"))
            .unwrap_or("Approval requested")
            .to_string(),
        tool: None,
        source_message_id: None,
        source_tool_call_id: string(item, "nodeId").map(str::to_string),
        metadata: Some(json!({"options":item.get("options"),"protocol":2})),
        message: None,
        options,
    })
}

pub fn form(thread_id: &str, item: &Value) -> Option<FormRequest> {
    let mut fields = Vec::new();
    for question in rows(item, "questions") {
        let key = string(question, "id")?.to_string();
        let title = string(question, "question")?.to_string();
        let options: Vec<_> = rows(question, "options")
            .iter()
            .filter_map(|option| {
                Some(FormOption {
                    value: string(option, "value")
                        .or_else(|| string(option, "label"))?
                        .to_string(),
                    label: string(option, "label")?.to_string(),
                    description: string(option, "description").map(str::to_string),
                })
            })
            .collect();
        let required = question
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let description = string(question, "header").map(str::to_string);
        if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
            // The released answer contract is string-valued; do not expose a
            // multiselect until the App's array-to-provider encoding is defined.
            return None;
        }
        fields.push(FormField::String {
            key,
            title,
            description,
            required,
            when: vec![],
            placeholder: None,
            default: string(question, "initialAnswer").map(str::to_string),
            options,
            format: None,
            min_length: None,
            max_length: None,
            pattern: None,
            custom: question
                .get("allowCustomAnswer")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        });
    }
    if fields.is_empty() {
        return None;
    }
    Some(FormRequest {
        nonblocking: false,
        id: string(item, "requestId")?.to_string(),
        asid: thread_id.into(),
        title: string(item, "title")
            .unwrap_or("Input requested")
            .to_string(),
        fields,
    })
}

pub fn permissions(thread_id: &str, projection: &Value) -> Vec<PermissionRequest> {
    rows(projection, "runtimeRequests")
        .iter()
        .filter_map(|request| {
            let (_, item) = pending_request(projection, string(request, "id")?)?;
            if string(item, "type") != Some("approval_request") {
                return None;
            }
            permission(thread_id, item)
        })
        .collect()
}

pub fn forms(thread_id: &str, projection: &Value) -> Vec<FormRequest> {
    rows(projection, "runtimeRequests")
        .iter()
        .filter_map(|request| {
            let (_, item) = pending_request(projection, string(request, "id")?)?;
            if string(item, "type") != Some("user_input_request") {
                return None;
            }
            let mut form = form(thread_id, item)?;
            form.nonblocking = request
                .pointer("/responseCapability/type")
                .and_then(Value::as_str)
                == Some("message");
            Some(form)
        })
        .collect()
}

pub fn approval_decision(
    projection: &Value,
    id: &str,
    choice: PermissionDecision,
) -> Result<&'static str, AgentError> {
    let (_, item) = pending_request(projection, id)
        .ok_or_else(|| AgentError::RequestFailed("request is not actionable".into()))?;
    if string(item, "type") != Some("approval_request") {
        return Err(AgentError::RequestFailed("not an approval request".into()));
    }
    let wire = match choice {
        PermissionDecision::Allow => "accept",
        PermissionDecision::AllowAlways => "acceptAlways",
        PermissionDecision::Deny => "decline",
    };
    if !rows(item, "options")
        .iter()
        .any(|option| string(option, "decision") == Some(wire))
    {
        return Err(AgentError::Unsupported(
            "approval decision was not offered".into(),
        ));
    }
    Ok(wire)
}

pub fn validate_answers(projection: &Value, id: &str, answers: &Value) -> Result<(), AgentError> {
    let (request, item) = pending_request(projection, id)
        .ok_or_else(|| AgentError::RequestFailed("request is not actionable".into()))?;
    if string(item, "type") != Some("user_input_request") || form("", item).is_none() {
        return Err(AgentError::Unsupported("unsupported question form".into()));
    }
    let answers = answers
        .as_object()
        .ok_or_else(|| AgentError::RequestFailed("answers must be an object".into()))?;
    let questions = rows(item, "questions");
    for key in answers.keys() {
        if !questions.iter().any(|q| string(q, "id") == Some(key)) {
            return Err(AgentError::RequestFailed("unknown question".into()));
        }
    }
    for question in questions {
        let key = string(question, "id").unwrap_or("");
        let value = answers.get(key).and_then(Value::as_str);
        let editable_live = request
            .pointer("/responseCapability/type")
            .and_then(Value::as_str)
            == Some("live")
            && string(question, "initialAnswer").is_some();
        if question.get("required").and_then(Value::as_bool) != Some(false)
            && value.is_none_or(|v| !editable_live && v.trim().is_empty())
        {
            return Err(AgentError::RequestFailed(
                "answer each required question".into(),
            ));
        }
        if answers.contains_key(key) && value.is_none() {
            return Err(AgentError::RequestFailed("answers must be strings".into()));
        }
        if let Some(value) = value {
            if !rows(question, "options").is_empty()
                && question.get("allowCustomAnswer").and_then(Value::as_bool) == Some(false)
                && !rows(question, "options").iter().any(|option| {
                    string(option, "value").or_else(|| string(option, "label")) == Some(value)
                })
            {
                return Err(AgentError::RequestFailed("answer was not offered".into()));
            }
        }
    }
    Ok(())
}

pub fn row_id(row: &Value) -> Option<String> {
    Some(format!(
        "t3v2:{}:{}",
        string(row, "sourceThreadId")?,
        string(row, "sourceItemId")?
    ))
}

pub fn timeline(thread_id: &str, projection: &Value, seq: u64) -> Vec<TimelineItem> {
    let mut result = Vec::new();
    let mut visible: Vec<_> = rows(projection, "visibleTurnItems").iter().collect();
    visible.sort_by_key(|row| row.get("position").and_then(Value::as_u64).unwrap_or(0));
    for row in visible {
        let Some(item) = row.get("item") else {
            continue;
        };
        let Some(id) = row_id(row) else {
            continue;
        };
        let position = row.get("position").and_then(Value::as_u64).unwrap_or(0);
        let mut role = TimelineRole::Assistant;
        let part = match string(item, "type").unwrap_or("") {
            "user_message" => {
                role = TimelineRole::User;
                AgentPart::Text {
                    text: string(item, "text").unwrap_or("").to_string(),
                }
            }
            "assistant_message" => AgentPart::Text {
                text: string(item, "text").unwrap_or("").to_string(),
            },
            "reasoning" => AgentPart::Reasoning {
                text: string(item, "text").unwrap_or("").to_string(),
                duration_ms: None,
            },
            "approval_request" => {
                if !matches!(
                    string(item, "status"),
                    Some("pending" | "running" | "waiting")
                ) {
                    continue;
                }
                let Some(request_id) = string(item, "requestId") else {
                    continue;
                };
                if pending_request(projection, request_id).is_none() {
                    continue;
                }
                let Some(request) = permission(thread_id, item) else {
                    continue;
                };
                AgentPart::Approval { request }
            }
            "user_input_request" => {
                if !matches!(
                    string(item, "status"),
                    Some("pending" | "running" | "waiting")
                ) {
                    continue;
                }
                let Some(request_id) = string(item, "requestId") else {
                    continue;
                };
                let Some((runtime_request, _)) = pending_request(projection, request_id) else {
                    continue;
                };
                let Some(mut request) = form(thread_id, item) else {
                    continue;
                };
                request.nonblocking = runtime_request
                    .pointer("/responseCapability/type")
                    .and_then(Value::as_str)
                    == Some("message");
                AgentPart::Form { request }
            }
            "file_change" => AgentPart::Diff {
                file: string(item, "fileName").unwrap_or("").to_string(),
                diff: string(item, "diffStr").unwrap_or("").to_string(),
            },
            "todo_list" => AgentPart::Todo {
                items: rows(item, "steps")
                    .iter()
                    .map(|step| TodoItem {
                        text: string(step, "text").unwrap_or("").to_string(),
                        done: string(step, "status") == Some("completed"),
                    })
                    .collect(),
            },
            kind @ ("command_execution" | "dynamic_tool" | "file_search" | "web_search"
            | "subagent") => {
                let state = match string(item, "status") {
                    Some("completed") => ToolCallStatus::Completed,
                    Some("failed" | "cancelled" | "interrupted") => ToolCallStatus::Failed,
                    Some("idle" | "pending") => ToolCallStatus::Pending,
                    _ => ToolCallStatus::Running,
                };
                AgentPart::Tool(ToolCall {
                    id: string(item, "id").unwrap_or("").to_string(),
                    name: match kind {
                        "command_execution" => "shell",
                        "dynamic_tool" => string(item, "toolName").unwrap_or("tool"),
                        "subagent" => "subagent",
                        other => other,
                    }
                    .into(),
                    title: string(item, "title").map(str::to_string),
                    input: match kind {
                        "command_execution" => {
                            json!({"command":string(item,"input").unwrap_or("")})
                        }
                        "dynamic_tool" => item.get("input").cloned().unwrap_or(Value::Null),
                        "subagent" => json!({"prompt":item.get("prompt")}),
                        _ => json!({"pattern":item.get("pattern"),"patterns":item.get("patterns")}),
                    },
                    output: item
                        .get("output")
                        .or_else(|| item.get("results"))
                        .or_else(|| item.get("result"))
                        .cloned(),
                    content: None,
                    metadata: Some(json!({"exit":item.get("exitCode")})),
                    state,
                    status: state,
                    error: None,
                    child_session_id: string(item, "childThreadId").map(str::to_string),
                    background: false,
                    input_partial: None,
                    truncated: item.get("outputOmitted").and_then(Value::as_bool) == Some(true),
                    time: ToolTime {
                        created: None,
                        ran: string(item, "startedAt").and_then(ms_from_iso),
                        completed: string(item, "completedAt").and_then(ms_from_iso),
                    },
                })
            }
            "system_notice" | "run_interrupt_request" | "run_interrupt_result" => {
                role = TimelineRole::System;
                AgentPart::System {
                    text: string(item, "message").unwrap_or("").to_string(),
                    description: None,
                }
            }
            "notification" => {
                role = TimelineRole::System;
                AgentPart::System {
                    text: string(item, "summary").unwrap_or("").to_string(),
                    description: string(item, "detail").map(str::to_string),
                }
            }
            "proposed_plan" => AgentPart::Text {
                text: string(item, "markdown").unwrap_or("").to_string(),
            },
            "error" => {
                role = TimelineRole::System;
                AgentPart::System {
                    text: item
                        .pointer("/failure/message")
                        .and_then(Value::as_str)
                        .unwrap_or("Provider failed")
                        .to_string(),
                    description: item
                        .pointer("/failure/class")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }
            }
            "compaction" => {
                role = TimelineRole::System;
                AgentPart::System {
                    text: string(item, "summary")
                        .unwrap_or("Context compacted")
                        .to_string(),
                    description: None,
                }
            }
            _ => continue,
        };
        result.push(TimelineItem {
            id: id.clone(),
            message_id: format!("{position:020}:{id}"),
            role,
            part,
            seq,
            ordinal: 0,
            updated_ms: string(item, "updatedAt").and_then(ms_from_iso).unwrap_or(0),
            attachments: None,
            image_assets: None,
            generation: None,
        });
    }
    result
}

pub fn interrupt_run(projection: &Value) -> Result<&str, AgentError> {
    active_root_run(projection)
        .and_then(|run| string(run, "id"))
        .ok_or_else(|| AgentError::RequestFailed("no active root run to interrupt".into()))
}
