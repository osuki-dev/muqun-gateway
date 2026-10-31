//! Read-only recovery evidence from one exact Claude Code transcript, used
//! only to decide whether an idle ACP child may be released. It never loads
//! a session, holds Claude Code's lock or writes to its store.
//!
//! The main chain is followed back from the last chat record through
//! `parentUuid`; each typed human prompt starts one entry holding the prompt
//! and the final assistant message's text. Request blobs and other metadata
//! records are skipped by their leading `type` without being parsed.
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::native_sessions::{metadata_type, project_dirs, projects_root, valid_session_id};
use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::ports::agent::AgentError;

/// Larger transcripts are not inspected; their child is simply kept.
const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// A chat record longer than this cannot be verified.
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 200_000;

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Claude Code native recovery history is incomplete".into())
}

pub(super) async fn inspect(
    directory: &str,
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    if !valid_session_id(id) || !Path::new(directory).is_absolute() {
        return Err(unavailable());
    }
    let root = projects_root().ok_or_else(unavailable)?;
    let (directory, id, stop) = (directory.to_owned(), id.to_owned(), stop.clone());
    tokio::task::spawn_blocking(move || {
        let path = find_exact(&root, &directory, &id)?;
        inspect_file(&path, &id, &stop)
    })
    .await
    .map_err(|_| unavailable())?
}

fn find_exact(root: &Path, directory: &str, id: &str) -> Result<PathBuf, AgentError> {
    let mut found = project_dirs(root, directory)
        .into_iter()
        .map(|dir| dir.join(format!("{id}.jsonl")))
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|stat| stat.is_file()));
    match (found.next(), found.next()) {
        (Some(path), None) => Ok(path),
        _ => Err(unavailable()),
    }
}

enum Line {
    Complete,
    /// Longer than [`MAX_LINE`]; only its first bytes were kept.
    Oversized,
}

/// One line into `buffer` (without its newline), keeping at most
/// [`MAX_LINE`] bytes of it. `None` at a clean end of file.
fn next_line(
    reader: &mut impl BufRead,
    buffer: &mut Vec<u8>,
    total: &mut u64,
) -> Result<Option<Line>, AgentError> {
    buffer.clear();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().map_err(|_| unavailable())?;
        if available.is_empty() {
            // A transcript being appended to ends mid-record: not settled.
            return if buffer.is_empty() && !oversized {
                Ok(None)
            } else {
                Err(unavailable())
            };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let take = end.unwrap_or(available.len());
        if buffer.len() + take <= MAX_LINE {
            buffer.extend_from_slice(&available[..take]);
        } else {
            let room = MAX_LINE.saturating_sub(buffer.len());
            buffer.extend_from_slice(&available[..room.min(take)]);
            oversized = true;
        }
        let consumed = end.map_or(take, |index| index + 1);
        *total += consumed as u64;
        if *total > MAX_BYTES {
            return Err(unavailable());
        }
        reader.consume(consumed);
        if end.is_some() {
            return Ok(Some(if oversized {
                Line::Oversized
            } else {
                Line::Complete
            }));
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_compact_summary: bool,
    origin: Option<Value>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    content: Option<Value>,
}

enum Node {
    Human(String),
    Assistant {
        message: Option<String>,
        text: String,
    },
    Other,
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect(),
        ),
        _ => None,
    }
}

fn node(record: &Record) -> Node {
    let content = record.message.as_ref().and_then(|m| m.content.as_ref());
    match record.kind.as_deref() {
        Some("user")
            if !record.is_meta
                && !record.is_compact_summary
                // Interrupt markers and command output carry no human origin.
                && record
                    .origin
                    .as_ref()
                    .is_some_and(|origin| origin["kind"] == "human")
                && !content.and_then(Value::as_array).is_some_and(|parts| {
                    parts.iter().any(|part| part["type"] == "tool_result")
                }) =>
        {
            match content.and_then(content_text) {
                Some(text) if !text.is_empty() => Node::Human(text),
                _ => Node::Other,
            }
        }
        Some("assistant") => Node::Assistant {
            message: record.message.as_ref().and_then(|m| m.id.clone()),
            text: content.and_then(content_text).unwrap_or_default(),
        },
        _ => Node::Other,
    }
}

fn inspect_file(
    path: &Path,
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| unavailable())?;
    let before = file.metadata().map_err(|_| unavailable())?;
    if !before.is_file() || before.len() > MAX_BYTES {
        return Err(unavailable());
    }
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, &file);
    let mut buffer = Vec::new();
    let mut total = 0;
    let mut nodes: HashMap<String, (Option<String>, Node)> = HashMap::new();
    let mut leaf = None;
    let mut records = 0;
    while let Some(line) = next_line(&mut reader, &mut buffer, &mut total)? {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        if buffer.is_empty() {
            continue;
        }
        // Metadata (titles, request blobs, cost, queue) is not part of the chain.
        if metadata_type(&buffer).is_some() {
            continue;
        }
        if matches!(line, Line::Oversized) {
            return Err(unavailable());
        }
        records += 1;
        if records > MAX_RECORDS {
            return Err(unavailable());
        }
        let record: Record = serde_json::from_slice(&buffer).map_err(|_| unavailable())?;
        if record
            .session_id
            .as_deref()
            .is_some_and(|native| native != id)
        {
            return Err(unavailable());
        }
        let Some(uuid) = record.uuid.clone() else {
            continue;
        };
        if record.is_sidechain {
            continue;
        }
        let entry = (record.parent_uuid.clone(), node(&record));
        if nodes.insert(uuid.clone(), entry).is_some() {
            return Err(unavailable());
        }
        leaf = Some(uuid);
    }
    let after = std::fs::symlink_metadata(path).map_err(|_| unavailable())?;
    if after.len() != before.len()
        || after.modified().ok() != before.modified().ok()
        || total != before.len()
    {
        return Err(unavailable());
    }
    let Some(mut next) = leaf else {
        return Ok(NativeSnapshot::empty(id));
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(next.clone()) {
            return Err(unavailable());
        }
        let Some((parent, node)) = nodes.remove(&next) else {
            return Err(unavailable());
        };
        chain.push(node);
        match parent {
            Some(parent) => next = parent,
            None => break,
        }
    }
    chain.reverse();
    Ok(snapshot(id, chain))
}

fn snapshot(id: &str, chain: Vec<Node>) -> NativeSnapshot {
    let mut native = NativeSnapshot::empty(id);
    // (prompt, final assistant message id, its text)
    let mut turn: Option<(String, Option<String>, String)> = None;
    let finish = |native: &mut NativeSnapshot, turn: (String, Option<String>, String)| {
        native
            .entries
            .push(json!({"user": turn.0, "assistant": turn.2}));
        native.last_user = Some(turn.0);
        native.last_assistant = Some(turn.2);
    };
    for node in chain {
        match node {
            Node::Human(text) => {
                if let Some(done) = turn.take() {
                    finish(&mut native, done);
                }
                turn = Some((text, None, String::new()));
            }
            Node::Assistant { message, text } if !text.is_empty() => {
                if let Some((_, current, answer)) = &mut turn {
                    // Claude Code writes one record per content block.
                    if message.is_none() || *current != message {
                        answer.clear();
                        *current = message;
                    }
                    answer.push_str(&text);
                }
            }
            _ => {}
        }
    }
    if let Some(done) = turn {
        finish(&mut native, done);
    }
    native
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(lines: &[Value], tail: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("muqun-claude-rec-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        text.push_str(tail);
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn main_chain_turns_end_with_the_final_message_and_skip_blobs() {
        let id = "00000000-0000-4000-8000-000000000002";
        let user = |uuid: &str, parent: Option<&str>, text: &str| {
            json!({"type":"user","uuid":uuid,"parentUuid":parent,"sessionId":id,
                   "origin":{"kind":"human"},"message":{"content":[{"type":"text","text":text}]}})
        };
        let assistant = |uuid: &str, parent: &str, message: &str, text: &str| {
            json!({"type":"assistant","uuid":uuid,"parentUuid":parent,"sessionId":id,
                   "message":{"id":message,"content":[{"type":"text","text":text}]}})
        };
        let lines = [
            user("u1", None, "First"),
            assistant("a1", "u1", "m1", "Working"),
            json!({"type":"user","uuid":"t1","parentUuid":"a1","sessionId":id,
                   "message":{"content":[{"type":"tool_result","content":"ok"}]}}),
            assistant("a2", "t1", "m2", "Done"),
            assistant("a3", "a2", "m2", " now"),
            user("u2", Some("a3"), "Second"),
            assistant("a4", "u2", "m3", "PONG"),
            // A sidechain branch is not the main chain.
            json!({"type":"assistant","uuid":"s1","parentUuid":"u2","isSidechain":true,"sessionId":id}),
        ];
        let (dir, path) = write(
            &lines,
            "{\"type\":\"api-request-blob\",\"message\":\"not json\n",
        );
        let native = inspect_file(&path, id, &CancellationToken::new()).unwrap();
        assert_eq!(native.entries.len(), 2);
        assert_eq!(native.entries[0]["assistant"], "Done now");
        assert_eq!(native.last_user.as_deref(), Some("Second"));
        assert_eq!(native.last_assistant.as_deref(), Some("PONG"));
        // An unfinished append is not settled evidence.
        std::fs::write(&path, format!("{}\n{{\"type\":\"user\"", lines[0])).unwrap();
        assert!(inspect_file(&path, id, &CancellationToken::new()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
