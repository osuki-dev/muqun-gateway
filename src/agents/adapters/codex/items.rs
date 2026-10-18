//! Pure projections of the pinned app-server ThreadItem union.
use crate::agents::domain::*;
use serde_json::{json, Value};

fn present(value: &Value, key: &str) -> Option<Value> {
    value.get(key).filter(|v| !v.is_null()).cloned()
}
fn state(value: &Value) -> ToolCallStatus {
    match value["status"].as_str() {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed" | "declined" | "interrupted") => ToolCallStatus::Failed,
        _ => ToolCallStatus::Running,
    }
}
fn tool(value: &Value, name: String, input: Value, output: Option<Value>) -> ToolCall {
    let mut metadata = value.clone();
    if let Some(metadata) = metadata.as_object_mut() {
        for key in [
            "aggregatedOutput",
            "arguments",
            "result",
            "changes",
            "contentItems",
            "agentsStates",
            "receiverThreadIds",
        ] {
            metadata.remove(key);
        }
    }
    let state = state(value);
    ToolCall {
        id: value["id"].as_str().unwrap_or_default().into(),
        name,
        title: None,
        input,
        output,
        content: None,
        metadata: Some(metadata),
        state,
        status: state,
        error: value["error"]["message"]
            .as_str()
            .map(|message| AgentErrorInfo {
                name: "CodexToolError".into(),
                message: message.into(),
                status: None,
            }),
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated: false,
        time: ToolTime::default(),
    }
}

fn patch_counts(diff: &str) -> Option<(usize, usize)> {
    let mut in_hunk = false;
    let mut saw_hunk = false;
    let (mut added, mut removed) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
            saw_hunk = true;
            continue;
        }
        if in_hunk {
            if line.starts_with('+') {
                added += 1;
            }
            if line.starts_with('-') {
                removed += 1;
            }
        }
    }
    saw_hunk.then_some((added, removed))
}

pub(super) fn parts(message: &str, id: &str, value: &Value) -> Vec<(String, AgentPart)> {
    let kind = value["type"].as_str().unwrap_or_default();
    if kind == "collabAgentToolCall" || kind == "subAgentActivity" {
        return collaboration(message, id, value);
    }
    let mut call = match kind {
        "commandExecution" => {
            let mut call = tool(
                value,
                "shell".into(),
                json!({"command":value["command"],"cwd":value["cwd"]}),
                present(value, "aggregatedOutput"),
            );
            if let Some(exit) = value["exitCode"].as_i64() {
                call.metadata.as_mut().unwrap()["exit"] = json!(exit);
            }
            // A PTY processId identifies a process, not proof of detachment.
            call
        }
        "fileChange" => {
            let mut call = tool(
                value,
                "edit".into(),
                json!({"paths":value["changes"].as_array().into_iter().flatten().filter_map(|change| change["path"].as_str()).collect::<Vec<_>>()}),
                None,
            );
            let files: Vec<_> = value["changes"].as_array().into_iter().flatten().map(|change| {
                let diff = change["diff"].as_str().unwrap_or_default();
                let mut file = json!({"path":change["path"],"diff":change["diff"],"status":change["kind"]["type"]});
                // Counts derive only from the supplied unified patch, not the worktree.
                if let Some((added, removed)) = patch_counts(diff) {
                    file["additions"] = json!(added);
                    file["deletions"] = json!(removed);
                }
                if let Some(path) = change["kind"]["move_path"].as_str() { file["move_path"] = json!(path); }
                file
            }).collect();
            call.metadata.as_mut().unwrap()["files"] = json!(files);
            call
        }
        "mcpToolCall" => tool(
            value,
            format!(
                "{}:{}",
                value["server"].as_str().unwrap_or_default(),
                value["tool"].as_str().unwrap_or_default()
            ),
            value["arguments"].clone(),
            present(value, "result"),
        ),
        "dynamicToolCall" => {
            let name = match value["namespace"].as_str() {
                Some(namespace) => {
                    format!("{namespace}:{}", value["tool"].as_str().unwrap_or_default())
                }
                None => value["tool"].as_str().unwrap_or_default().into(),
            };
            tool(
                value,
                name,
                value["arguments"].clone(),
                present(value, "contentItems"),
            )
        }
        "webSearch" => tool(
            value,
            "web_search".into(),
            json!({"query":value["query"],"action":value["action"]}),
            present(value, "results"),
        ),
        "sleep" => {
            let mut call = tool(
                value,
                "sleep".into(),
                json!({"durationMs":value["durationMs"]}),
                None,
            );
            // Requested sleep time is not a measured execution duration.
            call.metadata
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("durationMs");
            call
        }
        "imageView" => tool(
            value,
            "image_view".into(),
            json!({"path":value["path"]}),
            None,
        ),
        "imageGeneration" => tool(
            value,
            "image_generation".into(),
            json!({"prompt":value["revisedPrompt"]}),
            present(value, "result"),
        ),
        "functionCallOutput" => tool(
            value,
            value["name"].as_str().unwrap_or("tool").into(),
            Value::Null,
            present(value, "output"),
        ),
        "enteredReviewMode" | "exitedReviewMode" => {
            return vec![(
                part_item_id(message, 0),
                AgentPart::Status {
                    text: value["review"].as_str().unwrap_or_default().into(),
                },
            )]
        }
        "contextCompaction" => {
            return vec![(
                part_item_id(message, 0),
                AgentPart::Compaction {
                    status: if value["status"] == "inProgress" {
                        CompactionStatus::Running
                    } else {
                        CompactionStatus::Completed
                    },
                    reason: None,
                    summary: None,
                    recent: None,
                    tokens: None,
                    cost: None,
                    error: None,
                },
            )]
        }
        _ => return vec![],
    };
    // Canonical history items without a lifecycle field are complete. Live
    // item/started/completed supplies the envelope lifecycle before projection.
    if value.get("status").is_none() {
        call.state = ToolCallStatus::Completed;
        call.status = call.state;
    }
    vec![(tool_item_id(message, id), AgentPart::Tool(call))]
}

fn collaboration(message: &str, id: &str, value: &Value) -> Vec<(String, AgentPart)> {
    let activity = value["type"] == "subAgentActivity";
    let targets: Vec<&str> = if activity {
        value["agentThreadId"].as_str().into_iter().collect()
    } else {
        value["receiverThreadIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    let mut targets = targets;
    targets.sort_unstable();
    targets.dedup();
    // A failed spawn or list operation can have no receiver; retain the call.
    let targets: Vec<Option<&str>> = if targets.is_empty() {
        vec![None]
    } else {
        targets.into_iter().map(Some).collect()
    };
    targets.into_iter().map(|target| {
        let mut call = tool(value,"subagent".into(),json!({"prompt":value["prompt"],"operation":value["tool"],"model":value["model"],"reasoningEffort":value["reasoningEffort"]}),None);
        call.child_session_id = target.map(str::to_owned);
        let metadata = call.metadata.as_mut().unwrap();
        metadata.as_object_mut().unwrap().remove("status");
        if !activity { metadata["collabStatus"] = value["status"].clone(); }
        let child_status = if activity {
            match value["kind"].as_str() {
                Some("started") => Some("running"),
                Some("completed") => Some("completed"),
                Some("interrupted") => Some("interrupted"),
                _ => None,
            }
        } else { target.and_then(|target| value["agentsStates"][target]["status"].as_str()) };
        if let Some(status) = child_status {
            metadata["status"] = json!(match status { "pendingInit" => "starting", "running" => "working", "errored" => "failed", other => other });
        }
        if let Some(target) = target {
            metadata["sessionID"] = json!(target);
            call.output = present(&value["agentsStates"][target],"message");
        }
        if activity {
            call.state = ToolCallStatus::Completed;
            call.status = call.state;
            call.title = value["agentPath"].as_str().map(str::to_owned);
        }
        let row_id = target.map(|target| format!("{}:child:{}:{target}",tool_item_id(message,id),target.len())).unwrap_or_else(|| tool_item_id(message,id));
        (row_id,AgentPart::Tool(call))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::mapper;

    fn mapped(value: Value) -> Vec<ToolCall> {
        mapper::item("thread", "turn", &value, 0)
            .into_iter()
            .map(|row| {
                let AgentPart::Tool(call) = row.part else {
                    panic!("expected tool")
                };
                call
            })
            .collect()
    }
    #[test]
    fn patch_counts_do_not_confuse_hunk_content_with_file_headers() {
        assert_eq!(
            patch_counts("--- a/f\n+++ b/f\n@@ -1 +1 @@\n--- content\n+++ content"),
            Some((1, 1))
        );
        assert_eq!(patch_counts("unknown patch format"), None);
        let call = mapped(json!({"type":"sleep","id":"sleep","durationMs":50})).remove(0);
        assert_eq!(call.input["durationMs"], 50);
        assert!(call.metadata.unwrap().get("durationMs").is_none());
    }
    #[test]
    fn shell_preserves_exit_duration_script_and_process_without_inventing_background() {
        let rows = mapped(
            json!({"type":"commandExecution","id":"shell","status":"completed","source":"unifiedExecStartup","command":"echo test","cwd":"/fixture","processId":"42","exitCode":7,"durationMs":125,"scriptPath":"scripts/check.py","pluginId":"fixture","aggregatedOutput":"test"}),
        );
        let call = &rows[0];
        assert_eq!(call.name, "shell");
        assert_eq!(call.output, Some(json!("test")));
        assert_eq!(call.state, ToolCallStatus::Completed);
        assert!(!call.background);
        assert!(call.time.is_empty());
        let meta = call.metadata.as_ref().unwrap();
        assert_eq!(meta["exit"], 7);
        assert_eq!(meta["durationMs"], 125);
        assert_eq!(meta["scriptPath"], "scripts/check.py");
        assert_eq!(meta["processId"], "42");
        assert!(meta.get("aggregatedOutput").is_none());
    }
    #[test]
    fn edit_preserves_patch_rename_and_counts_without_reading_the_worktree() {
        let rows = mapped(
            json!({"type":"fileChange","id":"edit","status":"completed","changes":[{"path":"old.rs","kind":{"type":"update","move_path":"new.rs"},"diff":"--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n"}]}),
        );
        let file = &rows[0].metadata.as_ref().unwrap()["files"][0];
        assert_eq!(file["path"], "old.rs");
        assert_eq!(file["move_path"], "new.rs");
        assert_eq!(file["additions"], 1);
        assert_eq!(file["deletions"], 1);
        assert!(file["diff"].as_str().unwrap().contains("+new"));
    }
    #[test]
    fn completed_collaboration_call_keeps_child_lifecycle_separate_and_rows_stable() {
        let mut wire = json!({"type":"collabAgentToolCall","id":"spawn","tool":"wait","status":"completed","senderThreadId":"thread","receiverThreadIds":["child-b","child-a"],"prompt":null,"agentsStates":{"child-a":{"status":"running","message":null},"child-b":{"status":"errored","message":"failed"}}});
        let first = mapper::item("thread", "turn", &wire, 0);
        wire["receiverThreadIds"] = json!(["child-a", "child-b"]);
        let second = mapper::item("thread", "turn", &wire, 0);
        assert_eq!(first, second);
        let calls = mapped(wire);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].child_session_id.as_deref(), Some("child-a"));
        assert_eq!(calls[0].state, ToolCallStatus::Completed);
        assert_eq!(calls[0].metadata.as_ref().unwrap()["status"], "working");
        assert_eq!(calls[1].metadata.as_ref().unwrap()["status"], "failed");
        assert_eq!(calls[1].output, Some(json!("failed")));
    }
    #[test]
    fn missing_child_state_is_not_tool_completion_and_activity_identity_is_exact() {
        let call = mapped(json!({"type":"collabAgentToolCall","id":"spawn","status":"completed","receiverThreadIds":["child"],"agentsStates":{}})).remove(0);
        assert!(call.metadata.unwrap().get("status").is_none());
        let call = mapped(json!({"type":"subAgentActivity","id":"activity","kind":"interacted","agentThreadId":"child","agentPath":"/root/review"})).remove(0);
        assert_eq!(call.child_session_id.as_deref(), Some("child"));
        assert_eq!(call.title.as_deref(), Some("/root/review"));
        assert!(call.metadata.unwrap().get("status").is_none());
    }
    #[test]
    fn dynamic_web_sleep_and_mcp_outputs_are_not_silently_dropped() {
        for wire in [
            json!({"type":"dynamicToolCall","id":"dynamic","tool":"fixture","status":"failed","arguments":{"a":1},"contentItems":[{"type":"inputText","text":"result"}]}),
            json!({"type":"webSearch","id":"web","query":"fixture","results":[{"url":"https://example.test"}]}),
            json!({"type":"sleep","id":"sleep","durationMs":50}),
            json!({"type":"mcpToolCall","id":"mcp","server":"fixture","tool":"read","status":"failed","error":{"message":"unavailable"}}),
        ] {
            assert_eq!(mapped(wire).len(), 1);
        }
        let call =
            mapped(json!({"type":"sleep","id":"sleep","durationMs":50,"status":"inProgress"}))
                .remove(0);
        assert_eq!(call.state, ToolCallStatus::Running);
    }
}
