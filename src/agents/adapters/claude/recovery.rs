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
use crate::agents::domain::TokensUsage;
use crate::agents::ports::agent::AgentError;

/// The whole-transcript read ([`main_chain`]) parses every record into
/// memory. Larger transcripts are not inspected: a loaded child is simply
/// kept, and a terminal-held one reports [`too_large`].
const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// A chat record longer than this cannot be verified.
const MAX_LINE: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 200_000;

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Claude Code native recovery history is incomplete".into())
}

fn too_large() -> AgentError {
    AgentError::NotAvailable(format!(
        "Claude Code transcript exceeds the Gateway's whole-transcript read limit ({} MiB, {} MiB per record, {MAX_RECORDS} records); open this session in the agent's terminal",
        MAX_BYTES >> 20,
        MAX_LINE >> 20
    ))
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
            return Err(too_large());
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

/// One main-chain record: its role for recovery and, when kept, its content.
struct Chained {
    uuid: String,
    node: Node,
    kind: Option<String>,
    message_id: Option<String>,
    content: Option<Value>,
}

/// The main chain, oldest first. `settled` demands a complete, unchanged
/// file (recovery evidence); otherwise a transcript still being appended is
/// read up to its last complete line (a live terminal's history).
fn main_chain(
    path: &Path,
    id: &str,
    stop: &CancellationToken,
    settled: bool,
) -> Result<Vec<Chained>, AgentError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| unavailable())?;
    let before = file.metadata().map_err(|_| unavailable())?;
    if !before.is_file() {
        return Err(unavailable());
    }
    if before.len() > MAX_BYTES {
        return Err(too_large());
    }
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, &file);
    let mut buffer = Vec::new();
    let mut total = 0;
    let mut nodes: HashMap<String, (Option<String>, Chained)> = HashMap::new();
    let mut leaf = None;
    let mut records = 0;
    loop {
        let line = match next_line(&mut reader, &mut buffer, &mut total) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            // A live transcript ends mid-append; its complete lines stand.
            Err(_) if !settled => break,
            Err(error) => return Err(error),
        };
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
            return Err(too_large());
        }
        records += 1;
        if records > MAX_RECORDS {
            return Err(too_large());
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
        let node = node(&record);
        let chained = Chained {
            uuid: uuid.clone(),
            node,
            kind: record.kind.clone(),
            message_id: record.message.as_ref().and_then(|m| m.id.clone()),
            content: if settled {
                None
            } else {
                record.message.and_then(|m| m.content)
            },
        };
        if nodes
            .insert(uuid.clone(), (record.parent_uuid, chained))
            .is_some()
        {
            return Err(unavailable());
        }
        leaf = Some(uuid);
    }
    if settled {
        let after = std::fs::symlink_metadata(path).map_err(|_| unavailable())?;
        if after.len() != before.len()
            || after.modified().ok() != before.modified().ok()
            || total != before.len()
        {
            return Err(unavailable());
        }
    }
    let Some(mut next) = leaf else {
        return Ok(Vec::new());
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(next.clone()) {
            return Err(unavailable());
        }
        let Some((parent, chained)) = nodes.remove(&next) else {
            return Err(unavailable());
        };
        chain.push(chained);
        match parent {
            Some(parent) => next = parent,
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

fn inspect_file(
    path: &Path,
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    let chain = main_chain(path, id, stop, true)?;
    Ok(snapshot(id, chain.into_iter().map(|c| c.node).collect()))
}

/// The ACP `session/update`s a `session/load` of this transcript would
/// replay, for the shared projection: typed prompts, assistant text and
/// thinking, tool calls with their results.
fn replay_updates(chain: &[Chained]) -> Vec<Value> {
    let mut updates = Vec::new();
    for record in chain {
        let parts = match &record.content {
            Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
            Some(Value::Array(parts)) => parts.clone(),
            _ => continue,
        };
        match (record.kind.as_deref(), &record.node) {
            (Some("user"), Node::Human(text)) => updates.push(json!({
                "sessionUpdate": "user_message_chunk",
                "messageId": record.uuid,
                "content": {"type": "text", "text": text},
            })),
            (Some("user"), _) => {
                for part in parts.iter().filter(|p| p["type"] == "tool_result") {
                    let output = match &part["content"] {
                        Value::String(text) => text.clone(),
                        Value::Array(items) => items
                            .iter()
                            .filter_map(|item| item["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        _ => String::new(),
                    };
                    updates.push(json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": part["tool_use_id"],
                        "status": if part["is_error"] == true { "failed" } else { "completed" },
                        "content": [{"type": "content", "content": {"type": "text", "text": output}}],
                    }));
                }
            }
            (Some("assistant"), _) => {
                let message = record
                    .message_id
                    .clone()
                    .unwrap_or_else(|| record.uuid.clone());
                for part in &parts {
                    match part["type"].as_str() {
                        Some("text") => updates.push(json!({
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": message,
                            "content": {"type": "text", "text": part["text"]},
                        })),
                        Some("thinking")
                            if part["thinking"].as_str().is_some_and(|t| !t.is_empty()) =>
                        {
                            updates.push(json!({
                                "sessionUpdate": "agent_thought_chunk",
                                "messageId": message,
                                "content": {"type": "text", "text": part["thinking"]},
                            }))
                        }
                        Some("tool_use") => updates.push(json!({
                            "sessionUpdate": "tool_call",
                            "toolCallId": part["id"],
                            "title": part["name"],
                            "name": part["name"],
                            "status": "in_progress",
                            "rawInput": part["input"],
                        })),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    updates
}

/// Maximum bytes read from the tail of a Claude Code transcript for instant
/// cold-open preview before ACP `session/load` finishes in the background.
const PREVIEW_TAIL_BYTES: u64 = 4 * 1024 * 1024;

/// Fast, bounded tail preview of a Claude Code transcript without starting an
/// ACP child process. Even on multi-hundred-MB transcripts this reads at most
/// [`PREVIEW_TAIL_BYTES`] from the end and stops walking `parentUuid` cleanly
/// at the start of the window.
fn tail_chain_from(
    path: &Path,
    id: &str,
    tail_bytes: u64,
    stop: &CancellationToken,
) -> Result<Vec<Chained>, AgentError> {
    use std::io::{Seek, SeekFrom};

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| unavailable())?;
    let meta = file.metadata().map_err(|_| unavailable())?;
    if !meta.is_file() {
        return Err(unavailable());
    }
    let len = meta.len();
    let skip_first = len > tail_bytes;
    if skip_first {
        file.seek(SeekFrom::Start(len - tail_bytes))
            .map_err(|_| unavailable())?;
    }
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, &file);
    let mut buffer = Vec::new();
    let mut total = 0u64;
    if skip_first {
        let _ = next_line(&mut reader, &mut buffer, &mut total);
    }
    let mut nodes: HashMap<String, (Option<String>, Chained)> = HashMap::new();
    let mut leaf = None;
    let mut records = 0usize;
    loop {
        if stop.is_cancelled() || total > tail_bytes.saturating_add(MAX_LINE as u64) {
            break;
        }
        let line = match next_line(&mut reader, &mut buffer, &mut total) {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        if buffer.is_empty() || metadata_type(&buffer).is_some() || matches!(line, Line::Oversized)
        {
            continue;
        }
        records += 1;
        if records > MAX_RECORDS {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Record>(&buffer) else {
            continue;
        };
        if record
            .session_id
            .as_deref()
            .is_some_and(|native| native != id)
        {
            continue;
        }
        let Some(uuid) = record.uuid.clone() else {
            continue;
        };
        if record.is_sidechain {
            continue;
        }
        let node = node(&record);
        let chained = Chained {
            uuid: uuid.clone(),
            node,
            kind: record.kind.clone(),
            message_id: record.message.as_ref().and_then(|m| m.id.clone()),
            content: record.message.and_then(|m| m.content),
        };
        nodes.insert(uuid.clone(), (record.parent_uuid, chained));
        leaf = Some(uuid);
    }
    let Some(mut next) = leaf else {
        return Ok(Vec::new());
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while seen.insert(next.clone()) {
        let Some((parent, chained)) = nodes.remove(&next) else {
            break;
        };
        chain.push(chained);
        match parent {
            Some(parent) => next = parent,
            None => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

/// Recent timeline items read directly from the tail of Claude Code's native
/// `.jsonl` file, used to paint cold sessions immediately while ACP wakes.
pub(crate) async fn tail_timeline(
    directory: &str,
    id: &str,
    limit: usize,
    stop: &CancellationToken,
) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
    if !valid_session_id(id) || !Path::new(directory).is_absolute() {
        return Ok(Vec::new());
    }
    let Some(root) = projects_root() else {
        return Ok(Vec::new());
    };
    let (directory, id, stop) = (directory.to_owned(), id.to_owned(), stop.clone());
    tokio::task::spawn_blocking(move || {
        let Ok(path) = find_exact(&root, &directory, &id) else {
            return Ok(Vec::new());
        };
        let chain = tail_chain_from(&path, &id, PREVIEW_TAIL_BYTES, &stop)?;
        let mut projection = crate::agents::adapters::acp::mapper::Projection::new(false);
        for update in replay_updates(&chain) {
            let _ = projection.update(&update);
        }
        let mut items = projection.timeline();
        if limit > 0 && items.len() > limit {
            items.drain(..items.len() - limit);
        }
        Ok(items)
    })
    .await
    .map_err(|_| unavailable())?
}

/// The timeline of a transcript another process is still writing, read-only.
pub(crate) async fn live_timeline(
    directory: &str,
    id: &str,
    stop: &CancellationToken,
) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
    if !valid_session_id(id) || !Path::new(directory).is_absolute() {
        return Err(unavailable());
    }
    let root = projects_root().ok_or_else(unavailable)?;
    let (directory, id, stop) = (directory.to_owned(), id.to_owned(), stop.clone());
    tokio::task::spawn_blocking(move || {
        let Ok(path) = find_exact(&root, &directory, &id) else {
            return Ok(Vec::new());
        };
        let chain = main_chain(&path, &id, &stop, false)?;
        let mut projection = crate::agents::adapters::acp::mapper::Projection::new(false);
        for update in replay_updates(&chain) {
            // A record this reader cannot render is skipped, not fatal.
            let _ = projection.update(&update);
        }
        Ok(projection.timeline())
    })
    .await
    .map_err(|_| unavailable())?
}

/// How much of a transcript's end is read for its last model.
const MODEL_TAIL: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ran {
    #[serde(rename = "type")]
    kind: Option<String>,
    requested_model: Option<String>,
    effort: Option<String>,
    message: Option<RanMessage>,
}

#[derive(Deserialize)]
struct RanMessage {
    model: Option<String>,
}

/// The model and effort of the transcript's last assistant record, from
/// its final megabyte; Claude Code records them on every response.
fn last_model_in(bytes: &[u8], skip_first: bool) -> Option<(String, Option<String>)> {
    let mut lines = bytes.split(|byte| *byte == b'\n');
    if skip_first {
        lines.next();
    }
    let mut found = None;
    for line in lines {
        if line.is_empty() || metadata_type(line).is_some() {
            continue;
        }
        let Ok(ran) = serde_json::from_slice::<Ran>(line) else {
            continue;
        };
        if ran.kind.as_deref() != Some("assistant") {
            continue;
        }
        let model = ran
            .requested_model
            .or_else(|| ran.message.and_then(|message| message.model))
            .filter(|model| !model.is_empty() && model.len() <= 128);
        if let Some(model) = model {
            found = Some((model, ran.effort.filter(|effort| effort.len() <= 32)));
        }
    }
    found
}

pub(super) async fn transcript_model(
    directory: &str,
    id: &str,
) -> Result<Option<(String, Option<String>)>, AgentError> {
    if !valid_session_id(id) || !Path::new(directory).is_absolute() {
        return Ok(None);
    }
    let Some(root) = projects_root() else {
        return Ok(None);
    };
    let (directory, id) = (directory.to_owned(), id.to_owned());
    Ok(tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let path = find_exact(&root, &directory, &id).ok()?;
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        let start = len.saturating_sub(MODEL_TAIL);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut bytes = Vec::new();
        file.take(MODEL_TAIL).read_to_end(&mut bytes).ok()?;
        last_model_in(&bytes, start > 0)
    })
    .await
    .ok()
    .flatten())
}

/// Per-message token counts, Claude Code's `message.usage`.
#[derive(Deserialize)]
struct Spent {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<SpentMessage>,
}

#[derive(Deserialize)]
struct SpentMessage {
    id: Option<String>,
    usage: Option<MessageUsage>,
}

#[derive(Clone, Deserialize)]
struct MessageUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Clone, Deserialize)]
struct OutputDetails {
    thinking_tokens: Option<u64>,
}

/// How far a transcript's usage has been read: complete lines up to
/// `offset`, each message counted once by id (its last record wins; a
/// streamed message is written once per content block).
#[derive(Default)]
struct UsageScan {
    file: Option<(u64, u64)>,
    offset: u64,
    messages: HashMap<String, MessageUsage>,
    observed: Option<(u64, std::time::SystemTime)>,
    cached_total: Option<TokensUsage>,
}

impl UsageScan {
    const USAGE_BUFFER: u64 = 2 * 1024 * 1024;

    /// Keep the small common case in a reusable buffer. Claude also writes
    /// request blobs and tool results of arbitrary size. Skip metadata in
    /// place and stream JSON for exceptional chat records.
    fn read(&mut self, reader: &mut (impl BufRead + std::io::Seek)) -> std::io::Result<()> {
        use std::io::{Read, SeekFrom};
        let mut line = Vec::new();
        let mut start = reader.stream_position()?;
        loop {
            line.clear();
            let prefix = reader
                .take(Self::USAGE_BUFFER)
                .read_until(b'\n', &mut line)? as u64;
            if prefix == 0 {
                break;
            }
            let kind = metadata_type(&line);
            let chat = kind.is_none() || kind == Some("assistant");
            let (spent, consumed) = if line.last() == Some(&b'\n') {
                (
                    chat.then(|| serde_json::from_slice::<Spent>(&line).ok())
                        .flatten(),
                    prefix,
                )
            } else {
                if prefix < Self::USAGE_BUFFER {
                    break;
                }
                let mut consumed = prefix;
                loop {
                    let bytes = reader.fill_buf()?;
                    if bytes.is_empty() {
                        return Ok(());
                    }
                    let newline = bytes.iter().position(|&b| b == b'\n');
                    let n = newline.map_or(bytes.len(), |at| at + 1);
                    reader.consume(n);
                    consumed += n as u64;
                    if newline.is_some() {
                        break;
                    }
                }
                let spent = if chat {
                    reader.seek(SeekFrom::Start(start))?;
                    let spent = serde_json::from_reader::<_, Spent>(reader.take(consumed)).ok();
                    reader.seek(SeekFrom::Start(start + consumed))?;
                    spent
                } else {
                    None
                };
                (spent, consumed)
            };
            self.offset += consumed;
            start += consumed;
            if let Some(Spent {
                kind: Some(kind),
                message:
                    Some(SpentMessage {
                        id: Some(id),
                        usage: Some(usage),
                    }),
            }) = spent
            {
                if kind == "assistant" {
                    self.messages.insert(id, usage);
                }
            }
        }
        Ok(())
    }

    /// Disjoint counters, as the neutral shape has them: thinking is
    /// reported inside `output_tokens` and is taken out of it.
    fn total(&self) -> TokensUsage {
        let mut total = TokensUsage::default();
        let add = |slot: &mut Option<u64>, value: Option<u64>| {
            if let Some(value) = value {
                *slot = Some(slot.unwrap_or(0).saturating_add(value));
            }
        };
        for usage in self.messages.values() {
            let thinking = usage
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.thinking_tokens)
                .filter(|thinking| *thinking <= usage.output_tokens);
            total.input = total.input.saturating_add(usage.input_tokens);
            total.output = total
                .output
                .saturating_add(usage.output_tokens - thinking.unwrap_or(0));
            add(&mut total.reasoning, thinking);
            add(&mut total.cache_read, usage.cache_read_input_tokens);
            add(&mut total.cache_write, usage.cache_creation_input_tokens);
        }
        total
    }
}

type UsageCache = std::sync::Mutex<HashMap<PathBuf, std::sync::Arc<std::sync::Mutex<UsageScan>>>>;
const MAX_USAGE_SCANS: usize = 64;

/// The session's cumulative token usage from its transcript. A transcript
/// only grows, so each call reads what was appended since the last one.
/// Subagent transcripts are separate sessions and are not counted.
pub(super) async fn transcript_usage(
    directory: &str,
    id: &str,
) -> Result<Option<TokensUsage>, AgentError> {
    static CACHE: std::sync::OnceLock<UsageCache> = std::sync::OnceLock::new();
    if !valid_session_id(id) || !Path::new(directory).is_absolute() {
        return Ok(None);
    }
    let Some(root) = projects_root() else {
        return Ok(None);
    };
    let (directory, id) = (directory.to_owned(), id.to_owned());
    Ok(tokio::task::spawn_blocking(move || {
        let path = find_exact(&root, &directory, &id).ok()?;
        let cache = CACHE.get_or_init(Default::default);
        let entry = {
            let mut entries = cache.lock().ok()?;
            if !entries.contains_key(&path) && entries.len() >= MAX_USAGE_SCANS {
                let evict = entries
                    .iter()
                    .find(|(_, entry)| std::sync::Arc::strong_count(entry) == 1)
                    .map(|(key, _)| key.clone())?;
                entries.remove(&evict);
            }
            entries.entry(path.clone()).or_default().clone()
        };
        let mut scan = entry.lock().ok()?;
        scan.read_file(&path).ok()?
    })
    .await
    .ok()
    .flatten())
}

impl UsageScan {
    fn read_file(&mut self, path: &Path) -> std::io::Result<Option<TokensUsage>> {
        use std::io::{Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let stat = file.metadata()?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt as _;
            Some((stat.dev(), stat.ino()))
        };
        #[cfg(not(unix))]
        let identity = None;
        let stamp = stat.modified().ok().map(|mtime| (stat.len(), mtime));
        if self.file == identity && stamp.is_some() && self.observed == stamp {
            return Ok(self.cached_total.clone());
        }
        if self.file != identity
            || stat.len() < self.offset
            || self
                .observed
                .is_some_and(|(size, _)| size == stat.len() && stamp.is_some())
        {
            *self = UsageScan {
                file: identity,
                ..Default::default()
            };
        }
        file.seek(SeekFrom::Start(self.offset))?;
        self.read(&mut std::io::BufReader::with_capacity(128 * 1024, file))?;
        self.cached_total = (!self.messages.is_empty()).then(|| self.total());
        self.observed = stamp;
        Ok(self.cached_total.clone())
    }
}

/// The transcript's modification time, to notice that a terminal wrote to it.
pub(super) fn transcript_modified(directory: &str, id: &str) -> Option<std::time::SystemTime> {
    let root = projects_root()?;
    let path = find_exact(&root, directory, id).ok()?;
    std::fs::metadata(path).ok()?.modified().ok()
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
    fn the_last_assistant_record_names_the_model_and_effort_to_restore() {
        let lines = [
            json!({"type":"assistant","requestedModel":"claude-opus-5-5","effort":"high"}),
            json!({"type":"user","message":{"content":"next"}}),
            json!({"type":"assistant","message":{"model":"claude-haiku-5-5"},"effort":"low"}),
            json!({"type":"ai-title","aiTitle":"t"}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        assert_eq!(
            last_model_in(text.as_bytes(), false),
            Some(("claude-haiku-5-5".into(), Some("low".into())))
        );
        // A tail that starts mid-record never yields its partial first line.
        let cut = &text.as_bytes()[5..];
        assert_eq!(last_model_in(cut, true).unwrap().0, "claude-haiku-5-5");
        assert_eq!(last_model_in(b"{\"type\":\"user\"}\n", false), None);
    }

    #[test]
    fn usage_counts_each_message_once_and_resumes_after_a_partial_record() {
        let assistant = |id: &str, output: u64, thinking: u64| {
            json!({"type":"assistant","message":{"id":id,"usage":{
                "input_tokens":2,"output_tokens":output,
                "cache_creation_input_tokens":100,"cache_read_input_tokens":1000,
                "output_tokens_details":{"thinking_tokens":thinking}}}})
        };
        // One message per content block: the same id and usage, repeated.
        let first = [assistant("m1", 50, 20), assistant("m1", 50, 20)];
        let mut text: String = first.iter().map(|l| format!("{l}\n")).collect();
        text.push_str("{\"type\":\"api-request-blob\",\"usage\":{}}\n");
        let partial = format!("{}", assistant("m2", 30, 0));
        let (cut, rest) = partial.split_at(10);
        text.push_str(cut);
        let mut scan = UsageScan::default();
        scan.read(&mut std::io::Cursor::new(text.as_bytes()))
            .unwrap();
        let total = scan.total();
        assert_eq!(
            (
                total.input,
                total.output,
                total.reasoning,
                total.cache_read,
                total.cache_write
            ),
            (2, 30, Some(20), Some(1000), Some(100))
        );
        // The unfinished record is read whole once its line is complete.
        assert_eq!(scan.offset as usize, text.len() - cut.len());
        let tail = format!("{cut}{rest}\n");
        scan.read(&mut std::io::Cursor::new(tail.as_bytes()))
            .unwrap();
        let total = scan.total();
        assert_eq!(
            (total.input, total.output, total.reasoning),
            (4, 60, Some(20))
        );
    }

    #[test]
    fn usage_streams_long_records_and_discards_rotated_totals() {
        use std::io::Write;
        let (dir, path) = write(&[], "");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"api-request-blob","body":"x".repeat(UsageScan::USAGE_BUFFER as usize + 1)})
        )
        .unwrap();
        let assistant = |n| {
            json!({"type":"assistant", "message":{"id":"response",
            "content":"x".repeat(UsageScan::USAGE_BUFFER as usize + 1), "usage":{"input_tokens":n,"output_tokens":2}}})
        };
        writeln!(file, "{}", assistant(7)).unwrap();
        let mut scan = UsageScan::default();
        assert_eq!(scan.read_file(&path).unwrap().unwrap().input, 7);
        writeln!(file, "{}", assistant(9)).unwrap();
        assert_eq!(scan.read_file(&path).unwrap().unwrap().input, 9);
        let replacement = dir.join("new.jsonl");
        std::fs::write(&replacement, "{}\n").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(scan.read_file(&path).unwrap().is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The whole-transcript read is the one operation that still has a
    /// limit; it says so instead of a generic failure.
    #[test]
    fn whole_transcript_reads_name_their_limit_when_a_record_exceeds_it() {
        let id = "00000000-0000-4000-8000-000000000003";
        for (text_bytes, chained) in [(MAX_LINE / 2, Some(1)), (MAX_LINE, None)] {
            let line = json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":id,
                "origin":{"kind":"human"},"message":{"content":[{"type":"text","text":"x".repeat(text_bytes)}]}});
            let (dir, path) = write(&[line], "");
            match main_chain(&path, id, &CancellationToken::new(), false) {
                Ok(chain) => assert_eq!(Some(chain.len()), chained),
                Err(error) => {
                    let error = error.to_string();
                    assert!(
                        chained.is_none() && error.contains("whole-transcript read limit"),
                        "{error}"
                    );
                }
            }
            std::fs::remove_dir_all(dir).unwrap();
        }
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

    #[test]
    fn tail_timeline_reads_recent_turns_when_tail_window_truncates_earlier_history() {
        let id = "00000000-0000-4000-8000-000000000003";
        let big_pad = "x".repeat(4096);
        let lines = [
            json!({"type":"user","uuid":"u1","parentUuid":null,"sessionId":id,
                   "origin":{"kind":"human"},"message":{"content":[{"type":"text","text":big_pad}]}}),
            json!({"type":"assistant","uuid":"a1","parentUuid":"u1","sessionId":id,
                   "message":{"id":"m1","content":[{"type":"text","text":big_pad}]}}),
            json!({"type":"user","uuid":"u2","parentUuid":"a1","sessionId":id,
                   "origin":{"kind":"human"},"message":{"content":[{"type":"text","text":"Recent question"}]}}),
            json!({"type":"assistant","uuid":"a2","parentUuid":"u2","sessionId":id,
                   "message":{"id":"m2","content":[{"type":"text","text":"Recent answer"}]}}),
        ];
        let (dir, path) = write(&lines, "");
        // Window smaller than the whole file, large enough for the final turn.
        let chain = tail_chain_from(&path, id, 1024, &CancellationToken::new()).unwrap();
        let mut projection = crate::agents::adapters::acp::mapper::Projection::new(false);
        for update in replay_updates(&chain) {
            let _ = projection.update(&update);
        }
        let timeline = projection.timeline();
        assert!(timeline.len() >= 2, "{timeline:?}");
        assert!(timeline.iter().any(|item| matches!(
            &item.part,
            crate::agents::domain::AgentPart::Text { text } if text.contains("Recent question")
        )));
        assert!(timeline.iter().any(|item| matches!(
            &item.part,
            crate::agents::domain::AgentPart::Text { text } if text.contains("Recent answer")
        )));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
