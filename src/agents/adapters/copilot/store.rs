//! Fast read-only tail preview of a GitHub Copilot CLI session's local
//! `~/.copilot/session-state/<id>/events.jsonl` transcript without spawning
//! `copilot --acp`.
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agents::domain::TimelineItem;
use crate::agents::ports::agent::AgentError;

const PREVIEW_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EVENTS: usize = 50_000;

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub(super) async fn tail_timeline(
    id: &str,
    limit: usize,
    stop: &CancellationToken,
) -> Result<Vec<TimelineItem>, AgentError> {
    if !valid_id(id) {
        return Ok(Vec::new());
    }
    let Some(root) = dirs::home_dir().map(|home| home.join(".copilot").join("session-state"))
    else {
        return Ok(Vec::new());
    };
    let path = root.join(id).join("events.jsonl");
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || Ok(tail_timeline_file(&path, limit, &stop)))
        .await
        .map_err(|_| AgentError::NotAvailable("Copilot preview read interrupted".into()))?
}

fn tail_timeline_file(path: &Path, limit: usize, stop: &CancellationToken) -> Vec<TimelineItem> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let Ok(mut file) = options.open(path) else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    if !meta.is_file() {
        return Vec::new();
    }
    let len = meta.len();
    let skip_first = len > PREVIEW_TAIL_BYTES;
    if skip_first
        && file
            .seek(SeekFrom::Start(len - PREVIEW_TAIL_BYTES))
            .is_err()
    {
        return Vec::new();
    }
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, &file);
    let mut line = String::new();
    if skip_first {
        let _ = reader.read_line(&mut line);
        line.clear();
    }
    let mut projection = crate::agents::adapters::acp::mapper::Projection::new(false);
    let mut count = 0usize;
    while count < MAX_EVENTS {
        if stop.is_cancelled() {
            break;
        }
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        count += 1;
        let Ok(event) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(kind) = event["type"].as_str() else {
            continue;
        };
        let data = &event["data"];
        match kind {
            "user.message" => {
                if let Some(content) = data["content"].as_str().filter(|t| !t.is_empty()) {
                    let _ = projection.update(&json!({
                        "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": content },
                    }));
                }
            }
            "assistant.message" => {
                let message_id = data["messageId"]
                    .as_str()
                    .or_else(|| event["id"].as_str())
                    .unwrap_or_default();
                if let Some(reasoning) = data["reasoningText"].as_str().filter(|t| !t.is_empty()) {
                    let _ = projection.update(&json!({
                        "sessionUpdate": "agent_thought_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": reasoning },
                    }));
                }
                if let Some(content) = data["content"].as_str().filter(|t| !t.is_empty()) {
                    let _ = projection.update(&json!({
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": content },
                    }));
                }
            }
            "tool.execution_start" => {
                if let Some(call_id) = data["toolCallId"].as_str().filter(|s| !s.is_empty()) {
                    let name = data["toolName"].as_str().unwrap_or("tool");
                    let title = data["toolTitle"].as_str().unwrap_or(name);
                    let _ = projection.update(&json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id,
                        "title": title,
                        "name": name,
                        "status": "in_progress",
                        "rawInput": data["arguments"],
                    }));
                }
            }
            "tool.execution_complete" => {
                if let Some(call_id) = data["toolCallId"].as_str().filter(|s| !s.is_empty()) {
                    let status = if data["success"].as_bool() == Some(false) {
                        "failed"
                    } else {
                        "completed"
                    };
                    let _ = projection.update(&json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": call_id,
                        "status": status,
                        "rawOutput": data["result"],
                    }));
                }
            }
            _ => {}
        }
    }
    let mut items = projection.timeline();
    if limit > 0 && items.len() > limit {
        items.drain(..items.len() - limit);
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::AgentPart;

    #[test]
    fn tail_timeline_parses_copilot_events_jsonl() {
        let dir = crate::test_state_dir().join(format!("copilot-tail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let lines = [
            json!({"type":"session.start","data":{}}),
            json!({"type":"user.message","id":"e1","data":{"content":"hello copilot"}}),
            json!({"type":"tool.execution_start","id":"e2","data":{"toolCallId":"tc1","toolName":"bash","toolTitle":"Running command","arguments":{"command":"ls"}}}),
            json!({"type":"tool.execution_complete","id":"e3","data":{"toolCallId":"tc1","success":true,"result":{"content":"ok"}}}),
            json!({"type":"assistant.message","id":"e4","data":{"messageId":"m1","content":"All done!","reasoningText":"Checked files"}}),
        ];
        let body = lines
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let items = tail_timeline_file(&path, 50, &CancellationToken::new());
        assert!(items.iter().any(|item| matches!(
            &item.part,
            AgentPart::Text { text } if text.contains("hello copilot")
        )));
        assert!(items.iter().any(|item| matches!(
            &item.part,
            AgentPart::Tool(call) if call.name == "bash"
        )));
        assert!(items.iter().any(|item| matches!(
            &item.part,
            AgentPart::Text { text } if text.contains("All done!")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
