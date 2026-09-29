//! T3 read-model and event payloads into the neutral agent domain.
//!
//! Vocabulary (see `docs/t3-protocol.md`):
//! - a T3 *project* is an `AgentProject`; a *thread* is a session, and the
//!   thread id is the session id;
//! - `OrchestrationThreadShell` (from the shell snapshot / `thread-upserted`)
//!   and `OrchestrationThread` (from a thread snapshot) both map to
//!   `AgentSessionInfo`;
//! - thread `messages` become text/reasoning parts, `activities` become tool
//!   cards, approvals, forms, todo lists and status rows;
//! - `approval.requested` activities are `PermissionRequest`s and
//!   `user-input.requested` activities are `FormRequest`s;
//! - `server.getConfig().providers` is the catalog: one `ProviderInfo` per
//!   provider instance and its models as `ProviderModelInfo`. T3 has no
//!   persona inside an agent, so the catalog's modes are empty; a thread's
//!   `runtimeMode` is a permission policy, not a mode, and stays out of
//!   the session row.

use std::collections::HashMap;

use serde_json::{json, Value};

use super::client::{ms_from_iso, ModelSelection};
use crate::agents::domain::{
    AgentCatalog, AgentErrorInfo, AgentPart, AgentProject, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, CatalogDefaults, CommandInfo, FormField, FormOption, FormRequest,
    ModelInfo, ModelRef, PermissionDecision, PermissionOption, PermissionRequest, ProviderInfo,
    ProviderModelInfo, SkillInfo, TimelineItem, TimelineRole, TodoItem, ToolCall, ToolCallStatus,
    ToolTime,
};
use crate::agents::ports::agent::FileDiffItem;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}

fn ms(v: &Value, key: &str) -> Option<u64> {
    s(v, key).and_then(ms_from_iso)
}

// ---- projects and sessions ------------------------------------------------

/// `OrchestrationProjectShell` / `OrchestrationProject` -> `AgentProject`.
pub fn map_project(raw: &Value) -> Option<AgentProject> {
    let id = s(raw, "id")?;
    let root = s(raw, "workspaceRoot")?;
    let name = s(raw, "title").map(str::to_string).unwrap_or_else(|| {
        std::path::Path::new(root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.to_string())
    });
    let vcs = raw
        .get("repositoryIdentity")
        .filter(|r| r.is_object())
        .map(|_| "git".to_string());
    Some(AgentProject {
        id: id.to_string(),
        canonical: root.to_string(),
        name,
        vcs,
        sandboxes: vec![],
        missing: !std::path::Path::new(root).is_dir(),
    })
}

/// The domain model reference for a T3 `modelSelection`.
pub fn map_model_ref(raw: &Value) -> Option<ModelRef> {
    let sel = ModelSelection::from_value(raw)?;
    let variant = sel
        .options
        .iter()
        .find(|(id, _)| id == "effort" || id == "reasoningEffort")
        .and_then(|(_, v)| v.as_str().map(str::to_string));
    Some(ModelRef {
        provider_id: sel.instance_id,
        model_id: sel.model,
        variant,
    })
}

/// A domain `ModelRef` as the T3 wire selection. `variant` is carried as the
/// `effort` option, which is the id every current driver reads.
pub fn model_selection_from_ref(model: &ModelRef) -> ModelSelection {
    ModelSelection {
        instance_id: model.provider_id.clone(),
        model: model.model_id.clone(),
        options: model
            .variant
            .as_ref()
            .map(|v| vec![("effort".to_string(), json!(v))])
            .unwrap_or_default(),
    }
}

/// Where a thread stands, from its provider session and latest turn.
///
/// `hasPendingApprovals`/`hasPendingUserInput` (shell rows only) are folded
/// in as busy: the agent is waiting on the user, which the app shows the
/// same way as running.
pub fn map_status(thread: &Value) -> (AgentSessionStatus, Option<String>, Option<AgentErrorInfo>) {
    let session = thread.get("session").filter(|v| v.is_object());
    let latest = thread.get("latestTurn").filter(|v| v.is_object());
    let turn_state = latest.and_then(|t| s(t, "state"));
    let session_status = session.and_then(|v| s(v, "status"));
    let last_error = session.and_then(|v| s(v, "lastError"));

    let outcome = match turn_state {
        Some("completed") => Some("succeeded".to_string()),
        Some("error") => Some("failed".to_string()),
        Some("interrupted") => Some("interrupted".to_string()),
        _ => None,
    };
    let error = if matches!(turn_state, Some("error")) || matches!(session_status, Some("error")) {
        Some(AgentErrorInfo {
            name: "ProviderError".into(),
            message: last_error
                .unwrap_or("the provider reported an error")
                .to_string(),
            status: None,
        })
    } else {
        None
    };

    let flag = |key: &str| thread.get(key).and_then(Value::as_bool).unwrap_or(false);
    // A turn in flight, or the agent waiting on the user, both read as busy.
    let waiting = flag("hasPendingApprovals") || flag("hasPendingUserInput");
    let status = if matches!(turn_state, Some("running")) || waiting {
        AgentSessionStatus::Busy
    } else {
        match (turn_state, session_status) {
            (Some("error"), _) => AgentSessionStatus::Failed,
            (Some("interrupted"), _) => AgentSessionStatus::Interrupted,
            (_, Some("running")) | (_, Some("starting")) => AgentSessionStatus::Busy,
            (_, Some("error")) => AgentSessionStatus::Failed,
            (_, Some("interrupted")) => AgentSessionStatus::Interrupted,
            _ => AgentSessionStatus::Idle,
        }
    };
    (status, outcome, error)
}

/// `OrchestrationThreadShell` or `OrchestrationThread` -> `AgentSessionInfo`.
/// `projects` supplies the directory when the thread has no worktree.
pub fn map_session(thread: &Value, projects: &HashMap<String, String>) -> Option<AgentSessionInfo> {
    let id = s(thread, "id")?;
    let project_id = s(thread, "projectId");
    let directory = s(thread, "worktreePath")
        .map(str::to_string)
        .or_else(|| project_id.and_then(|p| projects.get(p).cloned()));
    let (status, outcome, error) = map_status(thread);
    let deleted = s(thread, "deletedAt").is_some();
    let updated_ms = ms(thread, "updatedAt")
        .or_else(|| ms(thread, "createdAt"))
        .unwrap_or(0);
    let time_idle = thread.get("latestTurn").and_then(|t| ms(t, "completedAt"));
    let title = s(thread, "title").unwrap_or("New thread").to_string();
    Some(AgentSessionInfo {
        asid: AgentSessionId(id.to_string()),
        backend_session_id: String::new(),
        agent_id: String::new(),
        title,
        mode: None,
        model: thread.get("modelSelection").and_then(map_model_ref),
        status,
        directory,
        cost: None,
        tokens: None,
        limit: None,
        parent_id: None,
        project_id: project_id.map(str::to_string),
        outcome,
        error,
        revert: None,
        fork: None,
        time_idle,
        time_viewed: None,
        deleted,
        updated_ms,
    })
}

/// A `projectId -> workspaceRoot` lookup from a shell or read-model snapshot.
pub fn project_roots(snapshot: &Value) -> HashMap<String, String> {
    snapshot
        .get("projects")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|p| Some((s(p, "id")?.to_string(), s(p, "workspaceRoot")?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Every thread of a shell snapshot, newest first, archived and deleted ones
/// left out.
pub fn map_shell_sessions(snapshot: &Value) -> Vec<AgentSessionInfo> {
    let roots = project_roots(snapshot);
    let mut sessions: Vec<AgentSessionInfo> = snapshot
        .get("threads")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|t| s(t, "archivedAt").is_none() && s(t, "deletedAt").is_none())
                .filter_map(|t| map_session(t, &roots))
                .collect()
        })
        .unwrap_or_default();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    sessions
}

// ---- timeline -------------------------------------------------------------

fn role_of(raw: &str) -> TimelineRole {
    match raw {
        "user" => TimelineRole::User,
        "system" => TimelineRole::System,
        _ => TimelineRole::Assistant,
    }
}

/// One `OrchestrationMessage` as a timeline row. `seq` orders rows that
/// share a timestamp; the caller passes the message's index.
pub fn map_message(message: &Value, seq: u64) -> Option<TimelineItem> {
    let id = s(message, "id")?;
    let role = s(message, "role").unwrap_or("assistant");
    let text = message
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let updated_ms = ms(message, "updatedAt")
        .or_else(|| ms(message, "createdAt"))
        .unwrap_or(0);
    let created_ms = ms(message, "createdAt").unwrap_or(updated_ms);
    let message_id = row_group(created_ms, id);
    let id = message_id.as_str();
    let attachments: Vec<String> = message
        .get("attachments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| s(a, "name").or_else(|| s(a, "path")).or_else(|| s(a, "id")))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let (item_id, part, timeline_role) = match role {
        "reasoning" => (
            crate::agents::domain::reasoning_item_id(id, 0),
            AgentPart::Reasoning {
                text,
                duration_ms: None,
            },
            TimelineRole::Assistant,
        ),
        "system" => (
            crate::agents::domain::part_item_id(id, 0),
            AgentPart::System {
                text,
                description: None,
            },
            TimelineRole::System,
        ),
        other => (
            crate::agents::domain::text_item_id(id, 0),
            AgentPart::Text { text },
            role_of(other),
        ),
    };
    Some(TimelineItem {
        id: item_id,
        message_id: message_id.clone(),
        role: timeline_role,
        part,
        seq,
        updated_ms,
        ordinal: 0,
        attachments: if attachments.is_empty() {
            None
        } else {
            Some(attachments)
        },
    })
}

/// The timeline `message_id` of a row group: the group's creation time
/// (milliseconds, zero-padded) ahead of the T3 id.
///
/// The contract orders a timeline by `(message_id, ordinal)` and promises
/// that message ids sort by creation. T3's ids do not: user messages are
/// UUIDs, assistant messages `assistant:<uuid>`, and activities have only a
/// turn id. The prefix makes them sort by creation; [`t3_message_id`]
/// takes it off again.
pub fn row_group(created_ms: u64, raw: &str) -> String {
    format!("{created_ms:013}:{raw}")
}

/// The T3 id inside a timeline `message_id` made by [`row_group`]; any other
/// string is returned as it is.
pub fn t3_message_id(message_id: &str) -> &str {
    let bytes = message_id.as_bytes();
    if bytes.len() > 14 && bytes[13] == b':' && bytes[..13].iter().all(u8::is_ascii_digit) {
        &message_id[14..]
    } else {
        message_id
    }
}

/// What ties successive activities to one timeline row: the tool call id
/// for tool cards, the turn for the plan. The row keeps the time its first
/// activity was created, so it stays where it started as it updates.
/// `None` for activities that are rows of their own.
pub fn activity_anchor_key(activity: &Value) -> Option<String> {
    let kind = s(activity, "kind")?;
    match kind {
        "tool.started" | "tool.updated" | "tool.completed" | "tool.denied" => {
            let payload = activity.get("payload")?;
            let id = s(payload, "toolCallId")
                .or_else(|| s(payload, "toolUseId"))
                .or_else(|| s(activity, "id"))?;
            Some(format!("tool:{id}"))
        }
        "turn.plan.updated" => Some(format!("plan:{}", activity_message_id(activity))),
        _ => None,
    }
}

/// The creation time of an activity, in milliseconds.
pub fn activity_created_ms(activity: &Value) -> u64 {
    ms(activity, "createdAt").unwrap_or(0)
}

/// The message id a turn's activities attach to: activities carry only a
/// `turnId`, so they are filed under a synthetic per-turn message.
pub fn activity_message_id(activity: &Value) -> String {
    match s(activity, "turnId") {
        Some(turn) => format!("turn:{turn}"),
        None => "turn:none".to_string(),
    }
}

fn tool_status(raw: Option<&str>) -> ToolCallStatus {
    match raw {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed") | Some("error") | Some("declined") => ToolCallStatus::Failed,
        Some("inProgress") | Some("in_progress") | Some("running") => ToolCallStatus::Running,
        _ => ToolCallStatus::Pending,
    }
}

/// Map an approval activity to a `PermissionRequest`.
pub fn map_permission(thread_id: &str, activity: &Value) -> Option<PermissionRequest> {
    let payload = activity.get("payload")?;
    let request_id = s(payload, "requestId")?;
    let kind = s(payload, "requestKind")
        .or_else(|| s(payload, "requestType"))
        .unwrap_or("permission");
    let detail = s(payload, "detail").unwrap_or("");
    let prompt = s(activity, "summary")
        .unwrap_or("Approval requested")
        .to_string();
    let mut options: Vec<PermissionOption> = payload
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .enumerate()
                .filter_map(|(index, o)| {
                    let decision = decision_from_t3(s(o, "decision")?)?;
                    Some(PermissionOption {
                        index,
                        label: s(o, "label").unwrap_or("Option").to_string(),
                        decision,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if options.is_empty() {
        options = vec![
            PermissionOption {
                index: 0,
                label: "Allow".into(),
                decision: PermissionDecision::Allow,
            },
            PermissionOption {
                index: 1,
                label: "Allow for this session".into(),
                decision: PermissionDecision::AllowAlways,
            },
            PermissionOption {
                index: 2,
                label: "Deny".into(),
                decision: PermissionDecision::Deny,
            },
        ];
    }
    Some(PermissionRequest {
        id: request_id.to_string(),
        asid: AgentSessionId(thread_id.to_string()),
        action: kind.to_string(),
        resources: if detail.is_empty() {
            vec![]
        } else {
            vec![detail.to_string()]
        },
        save: vec![],
        prompt,
        tool: s(payload, "appName").map(str::to_string),
        source_message_id: s(activity, "turnId").map(|t| format!("turn:{t}")),
        source_tool_call_id: None,
        metadata: Some(payload.clone()),
        message: if detail.is_empty() {
            None
        } else {
            Some(detail.to_string())
        },
        options,
    })
}

/// T3's approval decisions in the domain vocabulary.
pub fn decision_from_t3(raw: &str) -> Option<PermissionDecision> {
    match raw {
        "accept" => Some(PermissionDecision::Allow),
        "acceptForSession" | "acceptAlways" => Some(PermissionDecision::AllowAlways),
        "decline" | "cancel" => Some(PermissionDecision::Deny),
        _ => None,
    }
}

/// The T3 decision to send for a domain decision, preferring what the
/// request itself offered.
pub fn decision_to_t3(decision: PermissionDecision, offered: &[Value]) -> &'static str {
    let offers = |name: &str| offered.iter().any(|o| s(o, "decision") == Some(name));
    match decision {
        PermissionDecision::Allow => "accept",
        PermissionDecision::AllowAlways => {
            if offers("acceptAlways") {
                "acceptAlways"
            } else if offers("acceptForSession") || offered.is_empty() {
                "acceptForSession"
            } else {
                "accept"
            }
        }
        PermissionDecision::Deny => "decline",
    }
}

/// Map a `user-input.requested` activity to a `FormRequest`. Each question
/// is one field keyed by the question id; the answer record sent back is
/// `{ questionId: value }`.
pub fn map_form(thread_id: &str, activity: &Value) -> Option<FormRequest> {
    let payload = activity.get("payload")?;
    let request_id = s(payload, "requestId")?;
    let questions = payload.get("questions").and_then(Value::as_array)?;
    let fields: Vec<FormField> = questions
        .iter()
        .filter_map(|q| {
            let key = s(q, "id")?.to_string();
            let title = s(q, "question")
                .or_else(|| s(q, "header"))
                .unwrap_or("Question")
                .to_string();
            let description = s(q, "header").map(str::to_string);
            let options: Vec<FormOption> = q
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|o| {
                            let label = s(o, "label")?.to_string();
                            Some(FormOption {
                                value: s(o, "value").unwrap_or(&label).to_string(),
                                label,
                                description: s(o, "description").map(str::to_string),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let multi = q
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let custom = q
                .get("allowCustomAnswer")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || options.is_empty();
            Some(if multi && !options.is_empty() {
                FormField::Multiselect {
                    key,
                    title,
                    description,
                    required: true,
                    when: vec![],
                    options,
                    default: vec![],
                }
            } else {
                FormField::String {
                    key,
                    title,
                    description,
                    required: true,
                    when: vec![],
                    placeholder: None,
                    default: None,
                    options,
                    format: None,
                    min_length: None,
                    max_length: None,
                    pattern: None,
                    custom,
                }
            })
        })
        .collect();
    Some(FormRequest {
        id: request_id.to_string(),
        asid: AgentSessionId(thread_id.to_string()),
        title: s(activity, "summary")
            .unwrap_or("Input requested")
            .to_string(),
        fields,
    })
}

/// A tool activity (`tool.started|updated|completed|denied`) as a tool card.
/// Successive activities for one `toolCallId` share an item id, so the later
/// row replaces the earlier one when upserted.
pub fn map_tool(activity: &Value) -> Option<ToolCall> {
    let payload = activity.get("payload")?;
    let kind = s(activity, "kind")?;
    let id = s(payload, "toolCallId")
        .or_else(|| s(payload, "toolUseId"))
        .or_else(|| s(activity, "id"))?
        .to_string();
    let data = payload.get("data").filter(|d| d.is_object());
    let name = data
        .and_then(|d| s(d, "toolName"))
        .or_else(|| s(payload, "toolName"))
        .or_else(|| s(payload, "itemType"))
        .unwrap_or("tool")
        .to_string();
    let title = s(payload, "title")
        .or_else(|| s(activity, "summary"))
        .map(str::to_string);
    let mut input = data
        .map(|d| {
            let mut m = d.clone();
            if let Some(o) = m.as_object_mut() {
                o.remove("rawOutput");
                o.remove("toolName");
            }
            m
        })
        .unwrap_or(Value::Null);
    if input.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        if let Some(detail) = s(payload, "detail") {
            input = json!({ "detail": detail });
        }
    }
    let output = data
        .and_then(|d| d.get("rawOutput"))
        .map(|raw| match raw.get("content") {
            Some(c) => c.clone(),
            None => raw.clone(),
        });
    let state = match kind {
        "tool.denied" => ToolCallStatus::Failed,
        "tool.completed" => match s(payload, "status") {
            Some("failed") | Some("error") | Some("declined") => ToolCallStatus::Failed,
            _ => ToolCallStatus::Completed,
        },
        "tool.started" => ToolCallStatus::Running,
        _ => tool_status(s(payload, "status")),
    };
    let created = ms(activity, "createdAt");
    let error = match state {
        ToolCallStatus::Failed => Some(AgentErrorInfo {
            name: if kind == "tool.denied" {
                "ToolDenied"
            } else {
                "ToolFailed"
            }
            .into(),
            message: s(payload, "detail")
                .unwrap_or("the tool failed")
                .to_string(),
            status: None,
        }),
        _ => None,
    };
    Some(ToolCall {
        id,
        name,
        title,
        input,
        output: output.clone(),
        content: output.map(|o| match o {
            Value::String(text) => json!([{ "type": "text", "text": text }]),
            other => json!([{ "type": "text", "text": other.to_string() }]),
        }),
        metadata: Some(json!({
            "itemType": payload.get("itemType").cloned().unwrap_or(Value::Null),
            "toolSurface": payload.get("toolSurface").cloned().unwrap_or(Value::Null),
            "agentId": payload.get("agentId").cloned().unwrap_or(Value::Null),
        })),
        state,
        status: state,
        error,
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated: false,
        time: ToolTime {
            created,
            ran: if matches!(
                state,
                ToolCallStatus::Running | ToolCallStatus::Completed | ToolCallStatus::Failed
            ) {
                created
            } else {
                None
            },
            completed: if matches!(state, ToolCallStatus::Completed | ToolCallStatus::Failed) {
                created
            } else {
                None
            },
        },
    })
}

/// Which activity kinds produce a timeline row at all. The rest
/// (`context-window.updated`, `checkpoint.captured`, `approval.resolved`,
/// `user-input.resolved`, `task.*` bookkeeping) change state the session row
/// already carries or are noise for a phone.
pub fn activity_has_row(kind: &str) -> bool {
    matches!(
        kind,
        "tool.started"
            | "tool.updated"
            | "tool.completed"
            | "tool.denied"
            | "approval.requested"
            | "user-input.requested"
            | "turn.plan.updated"
            | "runtime.error"
            | "runtime.warning"
            | "checkpoint.revert.failed"
            | "runtime.note"
            | "context-compaction"
    )
}

/// One `OrchestrationThreadActivity` as a timeline row. `pending` says
/// whether an approval/user-input request is still open; resolved ones
/// produce no row.
///
/// `anchor_ms` is when the row's first activity was created (see
/// [`activity_anchor_key`]); `None` uses this activity's own time.
pub fn map_activity(
    thread_id: &str,
    activity: &Value,
    seq: u64,
    pending: bool,
    anchor_ms: Option<u64>,
) -> Option<TimelineItem> {
    let kind = s(activity, "kind")?;
    if !activity_has_row(kind) {
        return None;
    }
    let activity_id = s(activity, "id")?;
    let updated_ms = ms(activity, "createdAt").unwrap_or(0);
    let message_id = row_group(
        anchor_ms.unwrap_or(updated_ms),
        &activity_message_id(activity),
    );
    let payload = activity.get("payload").cloned().unwrap_or(Value::Null);
    let (id, part) = match kind {
        "tool.started" | "tool.updated" | "tool.completed" | "tool.denied" => {
            let mut tool = map_tool(activity)?;
            if anchor_ms.is_some() {
                tool.time.created = anchor_ms;
            }
            (
                crate::agents::domain::tool_item_id(&message_id, &tool.id),
                AgentPart::Tool(tool),
            )
        }
        "approval.requested" => {
            if !pending {
                return None;
            }
            let request = map_permission(thread_id, activity)?;
            (
                format!("{message_id}:approval:{}", request.id),
                AgentPart::Approval { request },
            )
        }
        "user-input.requested" => {
            if !pending {
                return None;
            }
            let request = map_form(thread_id, activity)?;
            (
                format!("{message_id}:form:{}", request.id),
                AgentPart::Form { request },
            )
        }
        "turn.plan.updated" => {
            let items = payload
                .get("plan")
                .and_then(Value::as_array)
                .map(|steps| {
                    steps
                        .iter()
                        .filter_map(|step| {
                            let text = s(step, "step").or_else(|| s(step, "title"))?.to_string();
                            let done =
                                matches!(s(step, "status"), Some("completed") | Some("done"));
                            Some(TodoItem { text, done })
                        })
                        .collect()
                })
                .unwrap_or_default();
            (format!("{message_id}:plan"), AgentPart::Todo { items })
        }
        _ => {
            let text = s(&payload, "message")
                .or_else(|| s(&payload, "detail"))
                .or_else(|| s(activity, "summary"))
                .unwrap_or(kind)
                .to_string();
            (
                format!("{message_id}:status:{activity_id}"),
                AgentPart::Status { text },
            )
        }
    };
    Some(TimelineItem {
        id,
        message_id,
        role: TimelineRole::Assistant,
        part,
        seq,
        updated_ms,
        ordinal: seq,
        attachments: None,
    })
}

/// Request ids of approvals/user-input requests that have been answered,
/// from the activities of a thread.
pub fn resolved_request_ids(activities: &[Value]) -> std::collections::HashSet<String> {
    activities
        .iter()
        .filter(|a| {
            matches!(
                s(a, "kind"),
                Some("approval.resolved") | Some("user-input.resolved")
            )
        })
        .filter_map(|a| {
            a.get("payload")
                .and_then(|p| s(p, "requestId"))
                .map(str::to_string)
        })
        .collect()
}

/// The full timeline of a thread snapshot (`OrchestrationThread`):
/// messages in order, then the activities that earn a row, sorted by time.
pub fn map_thread_timeline(thread_id: &str, thread: &Value) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut seq: u64 = 0;
    if let Some(messages) = thread.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(item) = map_message(message, seq) {
                items.push(item);
            }
            seq += 1;
        }
    }
    let activities: Vec<Value> = thread
        .get("activities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let resolved = resolved_request_ids(&activities);
    let mut tools_seen: HashMap<String, usize> = HashMap::new();
    let mut anchors: HashMap<String, u64> = HashMap::new();
    for activity in &activities {
        let pending = activity
            .get("payload")
            .and_then(|p| s(p, "requestId"))
            .map(|id| !resolved.contains(id))
            .unwrap_or(true);
        let anchor = activity_anchor_key(activity).map(|key| {
            *anchors
                .entry(key)
                .or_insert_with(|| activity_created_ms(activity))
        });
        if let Some(item) = map_activity(thread_id, activity, seq, pending, anchor) {
            // Later tool activities replace the earlier card in place.
            if let Some(index) = tools_seen.get(&item.id) {
                items[*index] = item;
            } else {
                tools_seen.insert(item.id.clone(), items.len());
                items.push(item);
            }
        }
        seq += 1;
    }
    items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    items
}

/// Open approvals of a thread snapshot.
pub fn map_pending_permissions(thread_id: &str, thread: &Value) -> Vec<PermissionRequest> {
    let activities: Vec<Value> = thread
        .get("activities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let resolved = resolved_request_ids(&activities);
    activities
        .iter()
        .filter(|a| s(a, "kind") == Some("approval.requested"))
        .filter_map(|a| map_permission(thread_id, a))
        .filter(|r| !resolved.contains(&r.id))
        .collect()
}

/// Open user-input requests of a thread snapshot.
pub fn map_pending_forms(thread_id: &str, thread: &Value) -> Vec<FormRequest> {
    let activities: Vec<Value> = thread
        .get("activities")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let resolved = resolved_request_ids(&activities);
    activities
        .iter()
        .filter(|a| s(a, "kind") == Some("user-input.requested"))
        .filter_map(|a| map_form(thread_id, a))
        .filter(|r| !resolved.contains(&r.id))
        .collect()
}

/// The highest `checkpointTurnCount` a thread has, which is what the diff
/// queries take as `toTurnCount`. `None` when nothing was checkpointed yet.
pub fn latest_checkpoint_turn(thread: &Value) -> Option<u64> {
    thread
        .get("checkpoints")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|c| c.get("checkpointTurnCount").and_then(Value::as_u64))
        .max()
}

/// The checkpoint turn count to revert to so that `target` and everything
/// after it are gone. `target` is a T3 message id or `turn:<turnId>`.
///
/// A turn starts with the user message that asked for it, so the turn count
/// to go back to is the number of user messages before that one. A
/// `turn:<id>` without a matching message falls back to the checkpoint taken
/// before that turn.
pub fn revert_turn_count(thread: &Value, target: &str) -> Option<u64> {
    let messages = thread
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let turn = target.strip_prefix("turn:");
    let position = messages.iter().position(|m| match turn {
        Some(turn) => s(m, "turnId") == Some(turn),
        None => s(m, "id") == Some(target),
    });
    if let Some(position) = position {
        // The user message that opened the turn `target` belongs to.
        let opener = messages[..=position]
            .iter()
            .rposition(|m| s(m, "role") == Some("user"))
            .unwrap_or(0);
        let before = messages[..opener]
            .iter()
            .filter(|m| s(m, "role") == Some("user"))
            .count() as u64;
        return Some(before);
    }
    let turn = turn?;
    thread
        .get("checkpoints")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| s(c, "turnId") == Some(turn))
        .and_then(|c| c.get("checkpointTurnCount").and_then(Value::as_u64))
        .map(|n| n.saturating_sub(1))
}

// ---- diffs ----------------------------------------------------------------

/// Split one unified diff (as `getTurnDiff`/`getFullThreadDiff` return it)
/// into per-file items with add/delete counts.
pub fn map_unified_diff(diff: &str) -> Vec<FileDiffItem> {
    let mut items: Vec<FileDiffItem> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .trim_end()
                .split(" b/")
                .last()
                .unwrap_or("")
                .trim_start_matches("a/")
                .to_string();
            items.push(FileDiffItem {
                path,
                patch: String::new(),
                additions: 0,
                deletions: 0,
            });
        }
        let Some(current) = items.last_mut() else {
            continue;
        };
        current.patch.push_str(line);
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            current.additions += 1;
        } else if line.starts_with('-') {
            current.deletions += 1;
        }
    }
    items
}

// ---- catalog --------------------------------------------------------------

/// `server.getConfig()` -> `AgentCatalog`.
///
/// Providers that are not installed or enabled are still listed (with
/// `activation: disabled`) so the app can explain what the host needs; their
/// models are left off the flat model list.
pub fn map_catalog(config: &Value) -> AgentCatalog {
    let mut models = Vec::new();
    let mut providers = Vec::new();
    let mut commands = Vec::new();
    let mut skills = Vec::new();
    let mut default_model: Option<ModelRef> = None;
    let mut seen_commands = std::collections::HashSet::new();

    if let Some(list) = config.get("providers").and_then(Value::as_array) {
        for p in list {
            let Some(instance_id) = s(p, "instanceId") else {
                continue;
            };
            let driver = s(p, "driver").unwrap_or(instance_id);
            let name = s(p, "displayName").unwrap_or(driver).to_string();
            let enabled = p.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let installed = p.get("installed").and_then(Value::as_bool).unwrap_or(false);
            let ready = enabled && installed && s(p, "status") != Some("disabled");
            let mut provider_models = Vec::new();
            if let Some(model_list) = p.get("models").and_then(Value::as_array) {
                for m in model_list {
                    let Some(slug) = s(m, "slug") else { continue };
                    let model_name = s(m, "name").unwrap_or(slug).to_string();
                    let is_default = m.get("isDefault").and_then(Value::as_bool).unwrap_or(false);
                    let status = if m.get("isLegacy").and_then(Value::as_bool).unwrap_or(false) {
                        Some("deprecated".to_string())
                    } else {
                        Some("active".to_string())
                    };
                    provider_models.push(ProviderModelInfo {
                        id: slug.to_string(),
                        name: model_name.clone(),
                        enabled: ready,
                        variants: vec![],
                        limit: None,
                        status: status.clone(),
                    });
                    if ready {
                        models.push(ModelInfo {
                            id: slug.to_string(),
                            name: model_name,
                            provider_id: instance_id.to_string(),
                            family: m
                                .get("capabilities")
                                .and_then(|c| s(c, "family"))
                                .map(str::to_string),
                            limit: None,
                            variants: None,
                            cost: None,
                            enabled: true,
                            status,
                        });
                        if is_default && default_model.is_none() {
                            default_model = Some(ModelRef {
                                provider_id: instance_id.to_string(),
                                model_id: slug.to_string(),
                                variant: None,
                            });
                        }
                    }
                }
            }
            if ready {
                if let Some(list) = p.get("slashCommands").and_then(Value::as_array) {
                    for c in list {
                        if let Some(cmd_name) = s(c, "name") {
                            if seen_commands.insert(cmd_name.to_string()) {
                                commands.push(CommandInfo {
                                    name: cmd_name.to_string(),
                                    description: s(c, "description").map(str::to_string),
                                    mode: None,
                                    template: None,
                                });
                            }
                        }
                    }
                }
                if let Some(list) = p.get("skills").and_then(Value::as_array) {
                    for sk in list {
                        if let Some(skill_name) = s(sk, "name") {
                            skills.push(SkillInfo {
                                id: format!("{instance_id}:{skill_name}"),
                                name: skill_name.to_string(),
                                description: s(sk, "description").unwrap_or("").to_string(),
                                slash: false,
                                autoinvoke: false,
                            });
                        }
                    }
                }
            }
            providers.push(ProviderInfo {
                id: instance_id.to_string(),
                name,
                activation: Some(if ready { "enabled" } else { "disabled" }.to_string()),
                models: provider_models,
            });
        }
    }

    // A first default: the first ready provider's first model.
    if default_model.is_none() {
        default_model = models.first().map(|m| ModelRef {
            provider_id: m.provider_id.clone(),
            model_id: m.id.clone(),
            variant: None,
        });
    }

    AgentCatalog {
        models,
        modes: vec![],
        mcp: vec![],
        skills,
        providers,
        commands,
        defaults: CatalogDefaults {
            model: default_model,
            mode: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "shell" => include_str!("fixtures/shell_snapshot.json"),
            "thread" => include_str!("fixtures/thread_detail.json"),
            "config" => include_str!("fixtures/server_config.json"),
            "approval" => include_str!("fixtures/activity_approval_requested.json"),
            "tool" => include_str!("fixtures/activity_tool_completed.json"),
            other => panic!("no fixture {other}"),
        };
        serde_json::from_str(raw).expect(name)
    }

    #[test]
    fn maps_projects_and_sessions_from_the_shell_snapshot() {
        let shell = fixture("shell");
        let projects: Vec<AgentProject> = shell["projects"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(map_project)
            .collect();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "t3proj");
        assert!(projects[0].canonical.ends_with("/t3proj"));
        assert!(
            projects[0].vcs.is_none(),
            "the probe repo has no remote, so T3 reports no repository identity"
        );
        let with_identity = json!({"id": "p", "title": "t", "workspaceRoot": "/nowhere/x",
            "repositoryIdentity": {"canonicalKey": "github.com/a/b", "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "…"}}});
        let project = map_project(&with_identity).unwrap();
        assert_eq!(project.vcs.as_deref(), Some("git"));
        assert!(
            project.missing,
            "a directory that is not on this host is flagged"
        );

        let sessions = map_shell_sessions(&shell);
        assert!(!sessions.is_empty());
        let first = &sessions[0];
        assert_eq!(first.model.as_ref().unwrap().provider_id, "claudeAgent");
        assert!(first.mode.is_none(), "T3 has no mode inside an agent");
        assert!(
            first.directory.as_deref().unwrap().ends_with("/t3proj"),
            "directory comes from the project"
        );
        assert!(first.backend_session_id.is_empty(), "asid is the thread id");
        assert!(
            sessions
                .windows(2)
                .all(|w| w[0].updated_ms >= w[1].updated_ms),
            "newest first"
        );
        let done = sessions
            .iter()
            .find(|s| s.outcome.is_some())
            .expect("a finished thread");
        assert_eq!(done.status, AgentSessionStatus::Idle);
        assert_eq!(done.outcome.as_deref(), Some("succeeded"));
        assert!(done.time_idle.is_some());
    }

    #[test]
    fn status_follows_the_turn_then_the_session() {
        let running = json!({"session": {"status": "running"}, "latestTurn": {"state": "running"}});
        assert_eq!(map_status(&running).0, AgentSessionStatus::Busy);
        let waiting = json!({"session": {"status": "running"}, "latestTurn": {"state": "running"}, "hasPendingApprovals": true});
        assert_eq!(map_status(&waiting).0, AgentSessionStatus::Busy);
        let interrupted =
            json!({"session": {"status": "ready"}, "latestTurn": {"state": "interrupted"}});
        let (status, outcome, _) = map_status(&interrupted);
        assert_eq!(status, AgentSessionStatus::Interrupted);
        assert_eq!(outcome.as_deref(), Some("interrupted"));
        let failed = json!({"session": {"status": "error", "lastError": "boom"}, "latestTurn": {"state": "error"}});
        let (status, _, error) = map_status(&failed);
        assert_eq!(status, AgentSessionStatus::Failed);
        assert_eq!(error.unwrap().message, "boom");
        let fresh = json!({"session": null, "latestTurn": null});
        assert_eq!(map_status(&fresh).0, AgentSessionStatus::Idle);
    }

    #[test]
    fn maps_the_thread_detail_into_a_timeline() {
        let detail = fixture("thread");
        let thread = &detail["thread"];
        let id = thread["id"].as_str().unwrap();
        let items = map_thread_timeline(id, thread);
        let user: Vec<_> = items
            .iter()
            .filter(|i| i.role == TimelineRole::User)
            .collect();
        assert_eq!(user.len(), 1);
        assert!(matches!(&user[0].part, AgentPart::Text { text } if text.contains("probe.txt")));
        let tools: Vec<_> = items
            .iter()
            .filter(|i| matches!(i.part, AgentPart::Tool(_)))
            .collect();
        assert_eq!(
            tools.len(),
            1,
            "started/updated/completed collapse into one card"
        );
        let AgentPart::Tool(tool) = &tools[0].part else {
            unreachable!()
        };
        assert_eq!(tool.name, "Write");
        assert_eq!(tool.state, ToolCallStatus::Completed);
        assert!(
            tool.id.starts_with("toolu_"),
            "the provider's tool call id: {}",
            tool.id
        );
        assert_eq!(
            t3_message_id(&tools[0].message_id),
            format!("turn:{}", thread["latestTurn"]["turnId"].as_str().unwrap())
        );
        // Rows sort by creation: the prompt, then the tool card, then the
        // reply, which T3's own ids would not give.
        let order: Vec<&str> = items
            .iter()
            .map(|i| match &i.part {
                AgentPart::Text { .. } if i.role == TimelineRole::User => "user",
                AgentPart::Tool(_) => "tool",
                AgentPart::Text { .. } => "assistant",
                _ => "other",
            })
            .collect();
        assert_eq!(order, ["user", "tool", "assistant"]);
        let mut sorted = items.clone();
        sorted.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        assert_eq!(sorted, items, "the mirror's order is the timeline's order");
        assert!(
            !items
                .iter()
                .any(|i| matches!(i.part, AgentPart::Approval { .. })),
            "a resolved approval has no row"
        );
        assert!(items.iter().any(|i| matches!(
            &i.part,
            AgentPart::Text { text } if i.role == TimelineRole::Assistant && text.to_lowercase().contains("done")
        )));
        assert!(
            items.windows(2).all(|w| w[0].updated_ms <= w[1].updated_ms),
            "chronological"
        );
        assert!(map_pending_permissions(id, thread).is_empty());
        assert!(map_pending_forms(id, thread).is_empty());
        assert_eq!(latest_checkpoint_turn(thread), Some(1));
        let info = map_session(thread, &HashMap::new()).unwrap();
        assert_eq!(info.asid.0, id);
        assert_eq!(info.outcome.as_deref(), Some("succeeded"));
    }

    #[test]
    fn maps_an_open_approval_to_a_permission_request() {
        let activity = fixture("approval");
        let request = map_permission("t1", &activity).unwrap();
        assert_eq!(
            request.id,
            activity["payload"]["requestId"].as_str().unwrap()
        );
        assert_eq!(request.action, "file-change");
        assert!(request.resources[0].starts_with("Write:"));
        assert_eq!(
            request.options.len(),
            3,
            "T3 offered no options, so the defaults are listed"
        );
        assert_eq!(request.options[1].decision, PermissionDecision::AllowAlways);
        let row = map_activity("t1", &activity, 5, true, None).unwrap();
        assert!(matches!(row.part, AgentPart::Approval { .. }));
        assert!(map_activity("t1", &activity, 5, false, None).is_none());
    }

    #[test]
    fn decisions_translate_both_ways() {
        assert_eq!(decision_from_t3("accept"), Some(PermissionDecision::Allow));
        assert_eq!(
            decision_from_t3("acceptAlways"),
            Some(PermissionDecision::AllowAlways)
        );
        assert_eq!(decision_from_t3("decline"), Some(PermissionDecision::Deny));
        assert_eq!(decision_from_t3("weird"), None);
        assert_eq!(decision_to_t3(PermissionDecision::Allow, &[]), "accept");
        assert_eq!(decision_to_t3(PermissionDecision::Deny, &[]), "decline");
        assert_eq!(
            decision_to_t3(PermissionDecision::AllowAlways, &[]),
            "acceptForSession"
        );
        let offered = vec![
            json!({"decision": "accept"}),
            json!({"decision": "acceptAlways"}),
        ];
        assert_eq!(
            decision_to_t3(PermissionDecision::AllowAlways, &offered),
            "acceptAlways"
        );
        let only_accept = vec![
            json!({"decision": "accept"}),
            json!({"decision": "decline"}),
        ];
        assert_eq!(
            decision_to_t3(PermissionDecision::AllowAlways, &only_accept),
            "accept"
        );
    }

    #[test]
    fn maps_a_user_input_request_to_a_form() {
        let activity = json!({
            "id": "a1", "tone": "info", "kind": "user-input.requested", "summary": "User input requested",
            "payload": { "requestId": "r1", "questions": [
                { "id": "q1", "header": "Scope", "question": "Which area?", "options": [
                    { "label": "Frontend", "description": "React" }, { "label": "Backend" }], "multiSelect": true },
                { "id": "q2", "header": "Name", "question": "What name?", "options": [], "allowCustomAnswer": true }
            ]},
            "turnId": "turn-1", "createdAt": "2026-09-29T08:35:04.754Z"
        });
        let form = map_form("t1", &activity).unwrap();
        assert_eq!(form.id, "r1");
        assert_eq!(form.fields.len(), 2);
        assert!(
            matches!(&form.fields[0], FormField::Multiselect { key, options, .. } if key == "q1" && options.len() == 2)
        );
        assert!(
            matches!(&form.fields[1], FormField::String { key, custom: true, .. } if key == "q2")
        );
        let row = map_activity("t1", &activity, 1, true, None).unwrap();
        assert_eq!(row.message_id, "1790670904754:turn:turn-1");
        assert_eq!(t3_message_id(&row.message_id), "turn:turn-1");
        assert!(matches!(row.part, AgentPart::Form { .. }));
    }

    #[test]
    fn maps_tool_activities_into_cards() {
        let completed = fixture("tool");
        let tool = map_tool(&completed).unwrap();
        assert_eq!(tool.name, "Write");
        assert_eq!(tool.state, ToolCallStatus::Completed);
        assert!(tool.input["detail"].as_str().unwrap().starts_with("Write:"));
        assert!(tool.time.completed.is_some());

        let running = json!({"id": "a", "kind": "tool.updated", "summary": "Command run", "payload": {
            "itemType": "command_execution", "toolCallId": "call-1", "status": "inProgress", "title": "Command run",
            "detail": "Bash: echo hi", "data": {"command": "echo hi", "toolName": "Bash", "rawOutput": {"content": "hi"}}},
            "turnId": "t", "createdAt": "2026-09-29T08:35:04.754Z"});
        let tool = map_tool(&running).unwrap();
        assert_eq!(tool.name, "Bash");
        assert_eq!(tool.state, ToolCallStatus::Running);
        assert_eq!(tool.input, json!({"command": "echo hi"}));
        assert_eq!(tool.output, Some(json!("hi")));
        assert_eq!(tool.content.unwrap()[0]["text"], "hi");

        let denied = json!({"id": "a", "kind": "tool.denied", "summary": "Tool denied: Bash", "payload": {
            "toolName": "Bash", "toolUseId": "call-2", "detail": "not allowed"}, "turnId": "t", "createdAt": "2026-09-29T08:35:04.754Z"});
        let tool = map_tool(&denied).unwrap();
        assert_eq!(tool.state, ToolCallStatus::Failed);
        assert_eq!(tool.error.unwrap().name, "ToolDenied");
    }

    #[test]
    fn plan_and_runtime_activities_have_rows_and_noise_does_not() {
        let plan = json!({"id": "p", "kind": "turn.plan.updated", "summary": "Plan updated", "payload": {"plan": [
            {"step": "Read", "status": "completed"}, {"step": "Write", "status": "in_progress"}]}, "turnId": "t", "createdAt": "2026-09-29T08:35:04.754Z"});
        let row = map_activity("t1", &plan, 0, true, None).unwrap();
        assert!(
            matches!(&row.part, AgentPart::Todo { items } if items.len() == 2 && items[0].done && !items[1].done)
        );
        // Observed live: a revert in a folder that is not a git repository
        // is accepted as a command and then fails as an activity; the row
        // is the only place the user learns why nothing was undone.
        let revert = json!({"id": "r", "tone": "error", "kind": "checkpoint.revert.failed", "summary": "Checkpoint revert failed",
            "payload": {"turnCount": 0, "detail": "Checkpoint workspace is unavailable or is not a git repository."},
            "turnId": null, "createdAt": "2026-09-29T08:35:04.754Z"});
        let row = map_activity("t1", &revert, 0, true, None).unwrap();
        assert!(
            matches!(&row.part, AgentPart::Status { text } if text.contains("not a git repository"))
        );
        let err = json!({"id": "e", "kind": "runtime.error", "summary": "Runtime error", "payload": {"message": "boom"}, "turnId": null, "createdAt": "2026-09-29T08:35:04.754Z"});
        let row = map_activity("t1", &err, 0, true, None).unwrap();
        assert!(matches!(&row.part, AgentPart::Status { text } if text == "boom"));
        assert_eq!(t3_message_id(&row.message_id), "turn:none");
        assert_eq!(
            t3_message_id("msg_1"),
            "msg_1",
            "a foreign id is left alone"
        );
        let ctx = json!({"id": "c", "kind": "context-window.updated", "summary": "x", "payload": {}, "turnId": "t", "createdAt": "2026-09-29T08:35:04.754Z"});
        assert!(map_activity("t1", &ctx, 0, true, None).is_none());
    }

    #[test]
    fn a_revert_target_resolves_to_the_turn_before_it() {
        let detail = fixture("thread");
        let thread = &detail["thread"];
        let items = map_thread_timeline("t", thread);
        // Every row of the only turn reverts to before it.
        for item in &items {
            assert_eq!(
                revert_turn_count(thread, t3_message_id(&item.message_id)),
                Some(0),
                "{}",
                item.message_id
            );
        }
        let two_turns = json!({"messages": [
            {"id": "u1", "role": "user"}, {"id": "a1", "role": "assistant", "turnId": "x"},
            {"id": "u2", "role": "user"}, {"id": "a2", "role": "assistant", "turnId": "y"}]});
        assert_eq!(revert_turn_count(&two_turns, "u2"), Some(1));
        assert_eq!(revert_turn_count(&two_turns, "a2"), Some(1));
        assert_eq!(revert_turn_count(&two_turns, "turn:x"), Some(0));
        assert_eq!(revert_turn_count(&two_turns, "nope"), None);
    }

    #[test]
    fn splits_a_unified_diff_per_file() {
        let diff = "diff --git a/probe.txt b/probe.txt\nnew file mode 100644\nindex 0000000..3087ae3\n--- /dev/null\n+++ b/probe.txt\n@@ -0,0 +1 @@\n+approval probe\n\\ No newline at end of file\ndiff --git a/src/x.rs b/src/x.rs\n--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n context\n";
        let items = map_unified_diff(diff);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].path, "probe.txt");
        assert_eq!((items[0].additions, items[0].deletions), (1, 0));
        assert!(items[0].patch.starts_with("diff --git a/probe.txt"));
        assert_eq!(items[1].path, "src/x.rs");
        assert_eq!((items[1].additions, items[1].deletions), (1, 1));
        assert!(map_unified_diff("").is_empty());
    }

    #[test]
    fn maps_the_server_config_into_a_catalog() {
        let config = fixture("config");
        let catalog = map_catalog(&config);
        let ready: Vec<_> = catalog
            .providers
            .iter()
            .filter(|p| p.activation.as_deref() == Some("enabled"))
            .collect();
        assert!(ready.iter().any(|p| p.id == "claudeAgent"));
        assert!(ready.iter().any(|p| p.id == "codex"));
        let disabled = catalog
            .providers
            .iter()
            .find(|p| p.id == "opencode")
            .unwrap();
        assert_eq!(disabled.activation.as_deref(), Some("disabled"));
        assert!(
            catalog
                .models
                .iter()
                .all(|m| m.provider_id == "claudeAgent" || m.provider_id == "codex"),
            "only ready providers' models are offered"
        );
        assert!(catalog
            .models
            .iter()
            .any(|m| m.provider_id == "claudeAgent" && m.id.starts_with("claude-")));
        let default = catalog.defaults.model.as_ref().unwrap();
        assert_eq!(
            default.provider_id, "codex",
            "the first ready provider's default model"
        );
        assert!(catalog.defaults.mode.is_none());
        assert!(catalog.modes.is_empty(), "T3 has no modes");
        assert!(
            !catalog.commands.is_empty(),
            "slash commands come from the providers"
        );
    }

    #[test]
    fn model_refs_carry_the_effort_option_as_the_variant() {
        let sel = json!({"instanceId": "codex", "model": "gpt-5.5", "options": [{"id": "effort", "value": "high"}]});
        let r = map_model_ref(&sel).unwrap();
        assert_eq!(r.variant.as_deref(), Some("high"));
        let back = model_selection_from_ref(&r);
        assert_eq!(back.to_value(), sel);
        let plain = model_selection_from_ref(&ModelRef {
            provider_id: "a".into(),
            model_id: "b".into(),
            variant: None,
        });
        assert!(plain.options.is_empty());
    }
}
