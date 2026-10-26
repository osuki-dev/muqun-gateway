//! Released rc.3 session-scoped skill/command metadata; no guessed defaults.
use crate::agents::domain::{CommandInfo, SkillInfo};
use crate::agents::ports::agent::AgentError;
use serde_json::{json, Value};
use std::collections::HashSet;

pub fn command_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

pub fn skills(raw: &Value) -> Result<Vec<SkillInfo>, AgentError> {
    let rows = raw
        .get("skills")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Protocol("invalid DeepSeek skill catalog".into()))?;
    let mut names = HashSet::new();
    rows.iter()
        .map(|row| {
            let name = row["name"]
                .as_str()
                .filter(|name| {
                    !name.is_empty() && !name.chars().any(|ch| ch.is_whitespace() || ch == '/')
                })
                .ok_or_else(|| AgentError::Protocol("invalid DeepSeek skill identity".into()))?;
            if !names.insert(name) {
                return Err(AgentError::Protocol(
                    "duplicate DeepSeek skill identity".into(),
                ));
            }
            Ok(SkillInfo {
                id: name.into(),
                name: name.into(),
                description: row["description"].as_str().unwrap_or("").into(),
                slash: true,
                autoinvoke: row["modelInvocable"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

pub fn commands(raw: &Value) -> Result<Vec<CommandInfo>, AgentError> {
    let rows = raw
        .as_array()
        .ok_or_else(|| AgentError::Protocol("invalid DeepSeek command catalog".into()))?;
    let mut names = HashSet::new();
    rows.iter()
        .map(|row| {
            let name = row["name"]
                .as_str()
                .filter(|name| command_name(name))
                .ok_or_else(|| AgentError::Protocol("invalid DeepSeek command identity".into()))?;
            if !names.insert(name) {
                return Err(AgentError::Protocol(
                    "duplicate DeepSeek command identity".into(),
                ));
            }
            Ok(CommandInfo {
                name: name.into(),
                description: row["description"].as_str().map(str::to_string),
                mode: None,
                template: None,
            })
        })
        .collect()
}

pub fn execution(raw: &Value) -> Result<Value, AgentError> {
    if raw.is_null() {
        return Err(AgentError::InvalidRequest(
            "DeepSeek command is no longer available".into(),
        ));
    }
    let id = raw["commandId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            AgentError::Protocol("DeepSeek command outcome is unknown; do not resend".into())
        })?;
    let result = &raw["result"];
    let kind = result["kind"]
        .as_str()
        .filter(|kind| matches!(*kind, "success" | "error"))
        .ok_or_else(|| AgentError::Protocol("invalid DeepSeek command result".into()))?;
    let mut mapped = json!({"command_id":id,"kind":kind});
    if let Some(text) = result["text"].as_str() {
        mapped["text"] = json!(text);
    } else if kind == "error" {
        return Err(AgentError::Protocol(
            "DeepSeek command failure lacks its reason".into(),
        ));
    }
    if let Some(seq) = result["sourceEventSeq"].as_u64() {
        mapped["source_event_seq"] = json!(seq);
    }
    Ok(json!({"submitted":true,"command_result":mapped}))
}
