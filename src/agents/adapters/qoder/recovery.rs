//! Recoverability inspection of one exact native Qoder transcript. This does
//! not load a session, hold its native lock, or write to the native store.
mod index;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::ports::agent::AgentError;

const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const PREVIEW_TAIL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Qoder native recovery history is incomplete or unsupported".into())
}

/// Fast tail preview of a Qoder `.jsonl` transcript directly from disk without
/// starting `qodercli --acp`.
pub(super) async fn tail_timeline(
    id: &str,
    limit: usize,
    stop: &CancellationToken,
) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(Vec::new());
    }
    let Some(root) = dirs::home_dir().map(|home| home.join(".qoder/projects")) else {
        return Ok(Vec::new());
    };
    let id = id.to_owned();
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || {
        let Ok(path) = find_exact(&root, &id, &stop) else {
            return Ok(Vec::new());
        };
        Ok(tail_timeline_file(&path, &id, limit, &stop).unwrap_or_default())
    })
    .await
    .map_err(|_| unavailable())?
}

fn tail_timeline_file(
    path: &Path,
    id: &str,
    limit: usize,
    stop: &CancellationToken,
) -> Result<Vec<crate::agents::domain::TimelineItem>, AgentError> {
    use std::io::{BufRead, Seek, SeekFrom};

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).map_err(|_| unavailable())?;
    let meta = file.metadata().map_err(|_| unavailable())?;
    if !meta.is_file() {
        return Ok(Vec::new());
    }
    let len = meta.len();
    let skip_first = len > PREVIEW_TAIL_BYTES;
    if skip_first {
        file.seek(SeekFrom::Start(len - PREVIEW_TAIL_BYTES))
            .map_err(|_| unavailable())?;
    }
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, &file);
    let mut line = String::new();
    if skip_first {
        let _ = reader.read_line(&mut line);
        line.clear();
    }
    let mut nodes: HashMap<String, (Option<String>, Value)> = HashMap::new();
    let mut leaf = None;
    let mut count = 0usize;
    while count < MAX_RECORDS {
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
        let Ok(record) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if record["sessionId"]
            .as_str()
            .is_some_and(|native| native != id)
            || record["isSidechain"].as_bool() == Some(true)
        {
            continue;
        }
        let Some(uuid) = record["uuid"].as_str().map(str::to_owned) else {
            continue;
        };
        let parent = record["parentUuid"].as_str().map(str::to_owned);
        nodes.insert(uuid.clone(), (parent, record));
        leaf = Some(uuid);
    }
    let Some(mut next) = leaf else {
        return Ok(Vec::new());
    };
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while seen.insert(next.clone()) {
        let Some((parent, record)) = nodes.remove(&next) else {
            break;
        };
        chain.push(record);
        match parent {
            Some(parent) => next = parent,
            None => break,
        }
    }
    chain.reverse();
    let mut projection = crate::agents::adapters::acp::mapper::Projection::new(false);
    for record in chain {
        if is_human(&record) {
            if let Some(text) = message_text(&record).filter(|t| !t.is_empty()) {
                let _ = projection.update(&serde_json::json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": { "type": "text", "text": text },
                }));
            }
        } else if record["type"].as_str() == Some("assistant") {
            let msg_id = record
                .pointer("/message/id")
                .and_then(Value::as_str)
                .or_else(|| record["uuid"].as_str())
                .unwrap_or_default();
            if let Some(parts) = record.pointer("/message/content").and_then(Value::as_array) {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => {
                            let _ = projection.update(&serde_json::json!({
                                "sessionUpdate": "agent_message_chunk",
                                "messageId": msg_id,
                                "content": { "type": "text", "text": part["text"] },
                            }));
                        }
                        Some("thinking")
                            if part["thinking"].as_str().is_some_and(|t| !t.is_empty()) =>
                        {
                            let _ = projection.update(&serde_json::json!({
                                "sessionUpdate": "agent_thought_chunk",
                                "messageId": msg_id,
                                "content": { "type": "text", "text": part["thinking"] },
                            }));
                        }
                        Some("tool_use") => {
                            let _ = projection.update(&serde_json::json!({
                                "sessionUpdate": "tool_call",
                                "toolCallId": part["id"],
                                "title": part["name"],
                                "name": part["name"],
                                "status": "completed",
                                "rawInput": part["input"],
                            }));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let mut items = projection.timeline();
    if limit > 0 && items.len() > limit {
        items.drain(..items.len() - limit);
    }
    Ok(items)
}

pub(super) async fn inspect(
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(unavailable());
    }
    let root = dirs::home_dir()
        .ok_or_else(unavailable)?
        .join(".qoder/projects");
    let id = id.to_owned();
    let stop = stop.child_token();
    let _cancel_read = stop.clone().drop_guard();
    inspect_owned(stop.clone(), move || {
        let path = find_exact(&root, &id, &stop)?;
        inspect_file(&path, &id, &stop)
    })
    .await
}

async fn inspect_owned<T: Send + 'static>(
    stop: CancellationToken,
    read: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    static READS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let reads = READS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone();
    let permit = tokio::select! {
        biased;
        _ = stop.cancelled() => return Err(unavailable()),
        permit = reads.acquire_owned() => permit.map_err(|_| unavailable())?,
    };
    tokio::task::spawn_blocking(move || {
        // A cancelled HTTP caller cannot release the byte-budget admission
        // while its blocking scan/read/parse is still using memory.
        let _permit = permit;
        read()
    })
    .await
    .map_err(|_| unavailable())?
}

fn find_exact(root: &Path, id: &str, stop: &CancellationToken) -> Result<PathBuf, AgentError> {
    let stat = std::fs::symlink_metadata(root).map_err(|_| unavailable())?;
    if stat.file_type().is_symlink() || !stat.is_dir() {
        return Err(unavailable());
    }
    let mut found = None;
    let filename = format!("{id}.jsonl");
    let mut entries = 0;
    for (index, bucket) in std::fs::read_dir(root)
        .map_err(|_| unavailable())?
        .enumerate()
    {
        if stop.is_cancelled() || index >= 1024 {
            return Err(unavailable());
        }
        let bucket = bucket.map_err(|_| unavailable())?;
        if !bucket.file_type().map_err(|_| unavailable())?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(bucket.path()).map_err(|_| unavailable())? {
            entries += 1;
            if stop.is_cancelled() || entries > 16_384 {
                return Err(unavailable());
            }
            let entry = entry.map_err(|_| unavailable())?;
            if entry.file_name() != filename.as_str() {
                continue;
            }
            if !entry.file_type().map_err(|_| unavailable())?.is_file() || found.is_some() {
                return Err(unavailable());
            }
            found = Some(entry.path());
        }
    }
    found.ok_or_else(unavailable)
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
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| unavailable())?;
    let before = file.metadata().map_err(|_| unavailable())?;
    if !before.is_file() || stop.is_cancelled() {
        return Err(unavailable());
    }
    let snapshot = inspect_reader(id, BufReader::new(&file), before.len(), stop)?;
    let after = file.metadata().map_err(|_| unavailable())?;
    let named = std::fs::symlink_metadata(path).map_err(|_| unavailable())?;
    let stable = after.len() == before.len()
        && named.len() == before.len()
        && before
            .modified()
            .ok()
            .zip(after.modified().ok())
            .is_some_and(|(a, b)| a == b)
        && before.modified().ok() == named.modified().ok()
        && !named.file_type().is_symlink()
        && named.is_file();
    #[cfg(unix)]
    let stable = {
        use std::os::unix::fs::MetadataExt;
        stable
            && before.dev() == named.dev()
            && before.ino() == named.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    };
    if !stable || stop.is_cancelled() {
        return Err(unavailable());
    }
    Ok(snapshot)
}

fn is_human(record: &Value) -> bool {
    record["type"].as_str() == Some("user")
        && record["isMeta"].as_bool() != Some(true)
        && record.pointer("/origin/kind").and_then(Value::as_str) == Some("human")
        && record.pointer("/humanInput/mode").and_then(Value::as_str) == Some("prompt")
        && record
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                !parts.is_empty()
                    && !parts
                        .iter()
                        .all(|part| part["type"].as_str() == Some("tool_result"))
            })
}

fn message_text(record: &Value) -> Option<String> {
    let parts = record.pointer("/message/content")?.as_array()?;
    let mut text = String::new();
    for part in parts {
        match part["type"].as_str()? {
            "text" => text.push_str(part["text"].as_str()?),
            // These native parts remain in the checkpoint, including their
            // complete content. Only text enters the live ACP text comparison.
            "thinking" | "redacted_thinking" | "tool_use" | "tool_result" | "image" => {}
            _ => return None,
        }
    }
    Some(text)
}

// Only the active ancestry counts as recoverable context. Keep locations,
// never tool payloads, for abandoned branches and out-of-order records.
struct Location {
    offset: u64,
    length: usize,
    parent: Result<Option<String>, ()>,
    human: bool,
}

fn active_branch(
    id: &str,
    mut reader: impl BufRead,
    length: u64,
    stop: &CancellationToken,
) -> Result<Option<index::Branch>, AgentError> {
    let mut records = index::Index::new();
    let mut active_leaf: Option<Option<String>> = None;
    let mut metadata_only = true;
    let mut line = Vec::new();
    let mut offset = 0;
    while offset < length {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        line.clear();
        let bytes = (&mut reader)
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| unavailable())?;
        let start = offset;
        offset += bytes as u64;
        if bytes as u64 > MAX_RECORD_BYTES || offset > length || !line.ends_with(b"\n") {
            return Err(unavailable());
        }
        if bytes == 1 {
            continue;
        }
        let record: Value = serde_json::from_slice(&line).map_err(|_| unavailable())?;
        if record["sessionId"]
            .as_str()
            .is_some_and(|native| native != id)
        {
            return Err(unavailable());
        }
        let kind = record["type"].as_str().ok_or_else(unavailable)?;
        if !matches!(
            kind,
            "user"
                | "assistant"
                | "attachment"
                | "active-leaf"
                | "runtime-config"
                | "custom-title"
                | "last-prompt"
                | "workspace-directories"
        ) {
            return Err(unavailable());
        }
        if matches!(kind, "user" | "assistant" | "attachment")
            && (!record["uuid"].is_string() || record["sessionId"].as_str() != Some(id))
        {
            return Err(unavailable());
        }
        let allowed: &[&str] = match kind {
            "active-leaf" => &[
                "type",
                "sessionId",
                "leafUuid",
                "explicit",
                "timestamp",
                "agentId",
            ],
            "runtime-config" => &[
                "type",
                "sessionId",
                "timestamp",
                "contextWindow",
                "generation",
                "model",
                "reasoningEffort",
            ],
            "custom-title" => &["type", "sessionId", "timestamp", "customTitle"],
            _ => &[],
        };
        if !allowed.is_empty()
            && record
                .as_object()
                .is_none_or(|object| object.keys().any(|key| !allowed.contains(&key.as_str())))
        {
            metadata_only = false;
        }

        if kind == "active-leaf"
            && record["sessionId"].as_str() == Some(id)
            && record["agentId"].is_null()
        {
            active_leaf = Some(match &record["leafUuid"] {
                Value::Null => None,
                Value::String(leaf) if !leaf.is_empty() && leaf.len() <= 256 => Some(leaf.clone()),
                _ => return Err(unavailable()),
            });
        }
        if !matches!(kind, "active-leaf" | "runtime-config" | "custom-title")
            || record["sessionId"].as_str() != Some(id)
            || record.get("parentSessionId").is_some()
        {
            metadata_only = false;
        }
        if let Some(uuid) = record["uuid"].as_str() {
            let parent = match &record["parentUuid"] {
                Value::Null => Ok(None),
                Value::String(parent) if !parent.is_empty() && parent.len() <= 256 => {
                    Ok(Some(parent.clone()))
                }
                _ => Err(()),
            };
            let location = Location {
                offset: start,
                length: bytes,
                parent,
                human: is_human(&record),
            };
            if uuid.is_empty() || uuid.len() > 256 {
                return Err(unavailable());
            }
            records.insert(uuid, location, stop)?;
        }
    }
    let Some(leaf) = active_leaf else {
        return Err(unavailable());
    };
    let Some(next) = leaf else {
        return if metadata_only && records.is_empty() {
            Ok(None)
        } else {
            Err(unavailable())
        };
    };
    records.trace(next, stop).map(Some)
}

fn inspect_reader(
    id: &str,
    mut reader: impl BufRead + Seek,
    length: u64,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    let Some(branch) = active_branch(id, &mut reader, length, stop)? else {
        return Ok(NativeSnapshot::empty(id));
    };
    let mut line = Vec::new();
    let current_turn = branch.current_turn;
    let mut position = None;
    let mut read_record = |offset, length, line: &mut Vec<u8>| -> Result<Value, AgentError> {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        if position != Some(offset) {
            reader
                .seek(SeekFrom::Start(offset))
                .map_err(|_| unavailable())?;
        }
        position = Some(offset + length as u64);
        line.resize(length, 0);
        reader.read_exact(line).map_err(|_| unavailable())?;
        serde_json::from_slice(line).map_err(|_| unavailable())
    };
    let (leaf_offset, leaf_length, _) = branch.at(branch.len - 1)?;
    let final_message = {
        let leaf = read_record(leaf_offset, leaf_length, &mut line)?;
        if leaf["type"].as_str() == Some("assistant")
            && leaf.pointer("/message/stop_reason").and_then(Value::as_str) == Some("end_turn")
            && leaf["aborted"].as_bool() != Some(true)
            && leaf["isApiErrorMessage"].as_bool() != Some(true)
        {
            Some(
                leaf.pointer("/message/id")
                    .and_then(Value::as_str)
                    .ok_or_else(unavailable)?
                    .to_owned(),
            )
        } else {
            None
        }
    };
    let mut last_assistant = final_message.as_ref().map(|_| String::new());
    let mut last_user = None;
    let mut entries = crate::agents::adapters::acp::recovery::RecoveryEntries::default();
    let mut turn = Sha256::new();
    for index in 0..branch.len {
        let (offset, size, human) = branch.at(index)?;
        let record = read_record(offset, size, &mut line)?;
        if record["sessionId"].as_str() != Some(id)
            || record["isSidechain"].as_bool() == Some(true)
            || !record["agentId"].is_null()
            || !matches!(
                record["type"].as_str(),
                Some("user" | "assistant" | "attachment")
            )
        {
            // Native compacted/repaired/sidechain formats need a separately
            // verified reader; a simple chain must not pretend to cover them.
            return Err(unavailable());
        }
        if record["type"].as_str() == Some("attachment")
            && !matches!(
                record.pointer("/attachment/type").and_then(Value::as_str),
                Some("skill_listing" | "environment" | "agent_listing_delta" | "session_context")
            )
        {
            return Err(unavailable());
        }
        if human {
            if index != 0 {
                entries.push(Value::String(hex::encode(turn.finalize_reset())));
            }
            let text = message_text(&record).ok_or_else(unavailable)?;
            if record.pointer("/humanInput/text").and_then(Value::as_str) != Some(text.as_str()) {
                return Err(unavailable());
            }
            if index == current_turn {
                last_user = Some(text);
            }
        }
        // Fixed-width hashes frame records unambiguously. Hash the complete
        // normalized JSON, including tool input, output and reasoning.
        line.clear();
        serde_json::to_writer(&mut line, &record).map_err(|_| unavailable())?;
        turn.update(Sha256::digest(&line));
        if index >= current_turn
            && record["type"].as_str() == Some("assistant")
            && final_message
                .as_deref()
                .is_some_and(|id| record.pointer("/message/id").and_then(Value::as_str) == Some(id))
        {
            if record["aborted"].as_bool() == Some(true)
                || record["isApiErrorMessage"].as_bool() == Some(true)
            {
                return Err(unavailable());
            }
            let text = message_text(&record).ok_or_else(unavailable)?;
            let joined = last_assistant.as_mut().ok_or_else(unavailable)?;
            if joined.len().saturating_add(text.len()) as u64 > MAX_RECORD_BYTES {
                return Err(unavailable());
            }
            joined.push_str(&text);
        }
    }
    entries.push(Value::String(hex::encode(turn.finalize())));
    Ok(NativeSnapshot {
        session_id: id.into(),
        entries: Vec::new(),
        streamed: Some(entries.finish()),
        last_user,
        last_assistant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn cancelling_the_waiter_does_not_admit_a_second_unfinished_read() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let first = tokio::spawn(inspect_owned(CancellationToken::new(), move || {
            let _ = entered.send(());
            blocked.recv().map_err(|_| unavailable())?;
            Ok(())
        }));
        started.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let stopped = CancellationToken::new();
        stopped.cancel();
        let cancelled = inspect_owned(stopped, || -> Result<(), AgentError> {
            panic!("cancelled reader started")
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), cancelled)
                .await
                .unwrap()
                .is_err()
        );
        let second_entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = second_entered.clone();
        let second = tokio::spawn(inspect_owned(CancellationToken::new(), move || {
            observed.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!second_entered.load(std::sync::atomic::Ordering::Acquire));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(second_entered.load(std::sync::atomic::Ordering::Acquire));
    }

    fn parse(id: &str, bytes: Vec<u8>) -> Result<NativeSnapshot, AgentError> {
        let length = bytes.len() as u64;
        inspect_reader(
            id,
            std::io::Cursor::new(bytes),
            length,
            &CancellationToken::new(),
        )
    }
    fn bytes(rows: &[Value]) -> Vec<u8> {
        rows.iter()
            .map(|row| serde_json::to_string(row).unwrap() + "\n")
            .collect::<String>()
            .into_bytes()
    }
    fn human(uuid: &str, parent: Value) -> Value {
        json!({"type":"user","sessionId":"same","uuid":uuid,"parentUuid":parent,"isSidechain":false,"origin":{"kind":"human"},"humanInput":{"mode":"prompt","text":"question"},"message":{"role":"user","content":[{"type":"text","text":"question"}]}})
    }
    fn assistant(uuid: &str, parent: &str, stop: Value, content: Value) -> Value {
        json!({"type":"assistant","sessionId":"same","uuid":uuid,"parentUuid":parent,"isSidechain":false,"message":{"id":"message","role":"assistant","stop_reason":stop,"content":content}})
    }
    #[tokio::test]
    #[ignore = "requires explicit QODER_RECOVERY_QA_ID belonging to an authorized completed QA session; read-only"]
    async fn real_qoder_recovery_snapshot_read_only() {
        let id = std::env::var("QODER_RECOVERY_QA_ID").expect("explicit own QA identity required");
        let snapshot = inspect(&id, &CancellationToken::new()).await.unwrap();
        assert_eq!(snapshot.session_id, id);
        assert!(snapshot
            .last_user
            .as_ref()
            .is_some_and(|text| !text.is_empty()));
        assert!(snapshot
            .last_assistant
            .as_ref()
            .is_some_and(|text| !text.is_empty()));
        println!(
            "verified native active branch: turns={}, checkpoint={}",
            snapshot.checkpoint().entries,
            snapshot.checkpoint().digest
        );
    }

    #[test]
    fn active_branch_groups_tool_results_into_the_real_human_turn() {
        let rows = vec![
            human("u", Value::Null),
            json!({"type":"attachment","sessionId":"same","uuid":"context","parentUuid":"u","attachment":{"type":"session_context","context":"native context"}}),
            assistant(
                "tool",
                "context",
                json!("tool_use"),
                json!([{"type":"tool_use","id":"call","name":"shell","input":{"command":"pwd"}}]),
            ),
            json!({"type":"user","sessionId":"same","uuid":"result","parentUuid":"tool","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"result"}]}}),
            assistant(
                "thinking",
                "result",
                Value::Null,
                json!([{"type":"thinking","thinking":"reason"}]),
            ),
            assistant(
                "final",
                "thinking",
                json!("end_turn"),
                json!([{"type":"text","text":"answer"}]),
            ),
            json!({"type":"active-leaf","sessionId":"same","leafUuid":"final"}),
            json!({"type":"runtime-config","sessionId":"same","model":"latest"}),
        ];
        let snapshot = parse("same", bytes(&rows)).unwrap();
        assert_eq!(snapshot.checkpoint().entries, 1);
        assert_eq!(snapshot.last_user.as_deref(), Some("question"));
        assert_eq!(snapshot.last_assistant.as_deref(), Some("answer"));
        let mut cancelled = rows.clone();
        cancelled
            .iter_mut()
            .find(|record| record["uuid"] == "final")
            .unwrap()["aborted"] = json!(true);
        assert!(parse("same", bytes(&cancelled))
            .unwrap()
            .last_assistant
            .is_none());
        let mut dangling = rows.clone();
        dangling
            .iter_mut()
            .find(|record| record["type"] == "active-leaf")
            .unwrap()["leafUuid"] = json!("missing");
        assert!(parse("same", bytes(&dangling)).is_err());
        let mut duplicate = rows.clone();
        duplicate.push(rows[0].clone());
        assert!(parse("same", bytes(&duplicate)).is_err());
        assert!(parse("other", bytes(&rows)).is_err());
    }
    #[test]
    fn large_active_history_proves_only_an_extension_of_the_original_turn() {
        use crate::agents::adapters::acp::recovery::{text_digest, TurnEvidence};
        use std::io::{BufWriter, Write};
        let dir =
            std::env::temp_dir().join(format!("muqun-qoder-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("same.jsonl");
        let first = vec![
            human("old", Value::Null),
            assistant(
                "old-answer",
                "old",
                json!("end_turn"),
                json!([{"type":"text","text":"first"}]),
            ),
            json!({"type":"active-leaf","sessionId":"same","leafUuid":"old-answer"}),
        ];
        std::fs::write(&path, bytes(&first)).unwrap();
        let stop = CancellationToken::new();
        let before = inspect_file(&path, "same", &stop).unwrap().checkpoint();
        {
            let mut writer = BufWriter::new(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap(),
            );
            writeln!(writer, "{}", human("new", json!("old-answer"))).unwrap();
            let mut parent = "new".to_string();
            let payload = "x".repeat(1024 * 1024);
            for i in 0..65 {
                let uuid = format!("result-{i}");
                writeln!(writer,r#"{{"type":"user","sessionId":"same","uuid":"{}","parentUuid":"{}","message":{{"content":[{{"type":"tool_result","content":"{}"}}]}}}}"#,uuid,parent,payload).unwrap();
                parent = uuid;
            }
            writeln!(
                writer,
                "{}",
                assistant(
                    "answer",
                    &parent,
                    json!("end_turn"),
                    json!([{"type":"text","text":"second"}])
                )
            )
            .unwrap();
            writeln!(
                writer,
                r#"{{"type":"active-leaf","sessionId":"same","leafUuid":"answer"}}"#
            )
            .unwrap();
            writer.flush().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > 64 * 1024 * 1024);
        let after = inspect_file(&path, "same", &stop).unwrap();
        let expected = TurnEvidence {
            user_digest: text_digest("question"),
            assistant_digest: text_digest("second"),
        };
        assert!(!after.is_empty());
        assert!(after.proves_turn("same", &before, &expected));
        let mut changed = first;
        changed[1]["message"]["content"][0]["text"] = json!("changed");
        let other = parse("same", bytes(&changed)).unwrap().checkpoint();
        assert!(!after.proves_turn("same", &other, &expected));
        assert!(!after.proves_turn("other", &before, &expected));
        assert!(inspect_file(&path, "other", &stop).is_err());
        stop.cancel();
        assert!(inspect_file(&path, "same", &stop).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn very_long_active_branch_proves_the_next_turn() {
        use crate::agents::adapters::acp::recovery::{text_digest, TurnEvidence};
        use std::io::{BufWriter, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.jsonl");
        let first = [
            human("first", Value::Null),
            assistant(
                "old",
                "first",
                json!("end_turn"),
                json!([{"type":"text","text":"first"}]),
            ),
            json!({"type":"active-leaf","sessionId":"same","leafUuid":"old"}),
        ];
        std::fs::write(&path, bytes(&first)).unwrap();
        let stop = CancellationToken::new();
        let before = inspect_file(&path, "same", &stop).unwrap().checkpoint();
        let mut out = BufWriter::new(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
        );
        writeln!(out, "{}", human("new", json!("old"))).unwrap();
        let mut parent = "new".to_string();
        for i in 0..100_001 {
            writeln!(out,r#"{{"type":"attachment","sessionId":"same","uuid":"n{i}","parentUuid":"{parent}","attachment":{{"type":"environment"}}}}"#).unwrap();
            parent = format!("n{i}");
        }
        writeln!(
            out,
            "{}",
            assistant(
                "final",
                &parent,
                json!("end_turn"),
                json!([{"type":"text","text":"second"}])
            )
        )
        .unwrap();
        writeln!(
            out,
            r#"{{"type":"active-leaf","sessionId":"same","leafUuid":"final"}}"#
        )
        .unwrap();
        out.flush().unwrap();
        let after = inspect_file(&path, "same", &stop).unwrap();
        assert!(after.proves_turn(
            "same",
            &before,
            &TurnEvidence {
                user_digest: text_digest("question"),
                assistant_digest: text_digest("second"),
            }
        ));
    }

    #[test]
    fn verified_empty_metadata_is_distinct_from_unknown_or_dangling_history() {
        let empty = vec![
            json!({"type":"active-leaf","sessionId":"same","leafUuid":null}),
            json!({"type":"runtime-config","sessionId":"same"}),
        ];
        assert!(parse("same", bytes(&empty)).unwrap().is_empty());
        let mut hidden = empty;
        hidden.push(human("u", Value::Null));
        assert!(parse("same", bytes(&hidden)).is_err());
    }
}
