//! Recoverability inspection of one exact native Qoder transcript. This does
//! not load a session, hold its native lock, or write to the native store.
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::ports::agent::AgentError;

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 100_000;

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Qoder native recovery history is incomplete or unsupported".into())
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
    let stop = stop.clone();
    inspect_owned(move || {
        let path = find_exact(&root, &id, &stop)?;
        inspect_file(&path, &id, &stop)
    })
    .await
}

async fn inspect_owned<T: Send + 'static>(
    read: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    static READS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = READS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| unavailable())?;
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
    let mut file = options.open(path).map_err(|_| unavailable())?;
    let before = file.metadata().map_err(|_| unavailable())?;
    if !before.is_file() || before.len() > MAX_BYTES || stop.is_cancelled() {
        return Err(unavailable());
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 != before.len() || !bytes.ends_with(b"\n") {
        return Err(unavailable());
    }
    let after = file.metadata().map_err(|_| unavailable())?;
    let named = std::fs::symlink_metadata(path).map_err(|_| unavailable())?;
    let stable = after.len() == before.len()
        && named.len() == before.len()
        && before
            .modified()
            .ok()
            .zip(after.modified().ok())
            .is_some_and(|(a, b)| a == b)
        && !named.file_type().is_symlink()
        && named.is_file();
    #[cfg(unix)]
    let stable = {
        use std::os::unix::fs::MetadataExt;
        stable && before.dev() == named.dev() && before.ino() == named.ino()
    };
    if !stable || stop.is_cancelled() {
        return Err(unavailable());
    }
    parse(id, bytes)
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

fn parse(id: &str, bytes: Vec<u8>) -> Result<NativeSnapshot, AgentError> {
    let mut records = HashMap::new();
    let mut active_leaf: Option<Option<String>> = None;
    let mut metadata_only = true;
    for (index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        if index >= MAX_RECORDS {
            return Err(unavailable());
        }
        let record: Value = serde_json::from_slice(line).map_err(|_| unavailable())?;
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
                Value::String(leaf) if !leaf.is_empty() => Some(leaf.clone()),
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
            if uuid.is_empty()
                || uuid.len() > 256
                || records.insert(uuid.to_owned(), record).is_some()
            {
                return Err(unavailable());
            }
        }
    }
    drop(bytes);
    let Some(leaf) = active_leaf else {
        return Err(unavailable());
    };
    let Some(mut next) = leaf else {
        return if metadata_only && records.is_empty() {
            Ok(NativeSnapshot::empty(id))
        } else {
            Err(unavailable())
        };
    };
    let mut seen = HashSet::new();
    let mut branch = Vec::new();
    loop {
        if !seen.insert(next.clone()) {
            return Err(unavailable());
        }
        let record = records.remove(&next).ok_or_else(unavailable)?;
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
        let parent = record["parentUuid"].clone();
        branch.push(record);
        match parent {
            Value::Null => break,
            Value::String(parent) if !parent.is_empty() => next = parent,
            _ => return Err(unavailable()),
        }
    }
    branch.reverse();
    let mut entries = Vec::new();
    let mut current = Vec::new();
    let mut last_user = None;
    for record in branch {
        if is_human(&record) {
            if !current.is_empty() {
                entries.push(Value::Array(std::mem::take(&mut current)));
            }
            let text = message_text(&record).ok_or_else(unavailable)?;
            if record.pointer("/humanInput/text").and_then(Value::as_str) != Some(text.as_str()) {
                return Err(unavailable());
            }
            last_user = Some(text);
        }
        current.push(record);
    }
    let last = current.last().ok_or_else(unavailable)?;
    let completed = last["type"].as_str() == Some("assistant")
        && last.pointer("/message/stop_reason").and_then(Value::as_str) == Some("end_turn")
        && last["aborted"].as_bool() != Some(true)
        && last["isApiErrorMessage"].as_bool() != Some(true);
    let last_assistant = if completed {
        let message_id = last
            .pointer("/message/id")
            .and_then(Value::as_str)
            .ok_or_else(unavailable)?;
        let mut text = String::new();
        for record in &current {
            if record["type"].as_str() == Some("assistant")
                && record.pointer("/message/id").and_then(Value::as_str) == Some(message_id)
            {
                if record["aborted"].as_bool() == Some(true)
                    || record["isApiErrorMessage"].as_bool() == Some(true)
                {
                    return Err(unavailable());
                }
                text.push_str(&message_text(record).ok_or_else(unavailable)?);
            }
        }
        Some(text)
    } else {
        None
    };
    entries.push(Value::Array(current));
    Ok(NativeSnapshot {
        session_id: id.into(),
        entries,
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
        let first = tokio::spawn(inspect_owned(move || {
            let _ = entered.send(());
            blocked.recv().map_err(|_| unavailable())?;
            Ok(())
        }));
        started.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let second_entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = second_entered.clone();
        let second = tokio::spawn(inspect_owned(move || {
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
            snapshot.entries.len(),
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
        assert_eq!(snapshot.entries.len(), 1);
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
    fn verified_empty_metadata_is_distinct_from_unknown_or_dangling_history() {
        let empty = vec![
            json!({"type":"active-leaf","sessionId":"same","leafUuid":null}),
            json!({"type":"runtime-config","sessionId":"same"}),
        ];
        assert!(parse("same", bytes(&empty)).unwrap().entries.is_empty());
        let mut hidden = empty;
        hidden.push(human("u", Value::Null));
        assert!(parse("same", bytes(&hidden)).is_err());
    }
}
