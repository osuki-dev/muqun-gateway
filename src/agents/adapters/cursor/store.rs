//! Read-only views of the Cursor CLI's ACP session folder,
//! `<config>/acp-sessions/<id>/`: `meta.json` (folder and title) for listing,
//! and `store.db` (a content-addressed blob store) as retirement evidence.
//! Chats started in Cursor's terminal UI live in a separate `chats/` store
//! that `agent acp` cannot open, so they are not read here. Nothing here
//! starts `agent` or writes to its files.
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::ports::agent::AgentError;

const MAX_ROWS: usize = 4096;
const MAX_MESSAGES: usize = 100_000;
const MAX_META_BYTES: u64 = 64 * 1024;
const MAX_TITLE_CHARS: usize = 200;
const QUERY_OPEN: &str = "<user_query>\n";
const QUERY_CLOSE: &str = "\n</user_query>";

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Cursor's ACP session store is unreadable".into())
}

/// The CLI's own rule: `CURSOR_CONFIG_DIR`, else `$XDG_CONFIG_HOME/cursor`,
/// else `~/.cursor`. The `agent acp` child inherits the same environment.
fn root() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let config = match (set("CURSOR_CONFIG_DIR"), set("XDG_CONFIG_HOME")) {
        (Some(dir), _) => PathBuf::from(dir),
        (None, Some(xdg)) => PathBuf::from(xdg).join("cursor"),
        (None, None) => dirs::home_dir()?.join(".cursor"),
    };
    Some(config.join("acp-sessions"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

async fn blocking<T: Send + 'static>(
    stop: &CancellationToken,
    read: impl FnOnce(CancellationToken) -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    let stop = stop.clone();
    tokio::task::spawn_blocking(move || read(stop))
        .await
        .map_err(|_| unavailable())?
}

/// Titled sessions as ACP `session/list` rows, newest first. `agent acp`
/// titles a session on its first prompt and lists only titled ones too.
pub(super) async fn list(
    directory: Option<&str>,
    stop: &CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    let Some(root) = root() else {
        return Ok(Vec::new());
    };
    let directory = directory.map(str::to_owned);
    blocking(stop, move |stop| {
        list_at(&root, directory.as_deref(), &stop)
    })
    .await
}

pub(super) fn list_at(
    root: &Path,
    directory: Option<&str>,
    stop: &CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(unavailable()),
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_id(&id) || !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let folder = entry.path();
        let Some((cwd, title)) = read_meta(&folder.join("meta.json")) else {
            continue;
        };
        if directory.is_some_and(|wanted| wanted != cwd) {
            continue;
        }
        let updated = ["store.db-wal", "store.db", "meta.json"]
            .iter()
            .filter_map(|name| modified_ms(&folder.join(name)))
            .max();
        sessions.push((
            updated.unwrap_or(0),
            json!({ "sessionId": id, "cwd": cwd, "title": title }),
        ));
    }
    sessions.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
    Ok(sessions
        .into_iter()
        .take(MAX_ROWS)
        .map(|(updated, mut row)| {
            if updated > 0 {
                row["updatedAt"] = json!(crate::agents::adapters::t3::client::iso_from_ms(updated));
            }
            row
        })
        .collect())
}

fn read_meta(path: &Path) -> Option<(String, String)> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_META_BYTES).read_to_end(&mut bytes).ok()?;
    let meta: Value = serde_json::from_slice(&bytes).ok()?;
    let cwd = meta["cwd"]
        .as_str()
        .filter(|cwd| Path::new(cwd).is_absolute())?;
    let title: String = meta["title"]
        .as_str()?
        .trim()
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    (!title.is_empty()).then(|| (cwd.to_owned(), title))
}

fn modified_ms(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// One entry per user prompt: its text, then each assistant message's text.
/// The last assistant message's text is what the live stream showed last.
pub(super) async fn recovery_snapshot(
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    if !valid_id(id) {
        return Err(unavailable());
    }
    let root = root().ok_or_else(unavailable)?;
    let id = id.to_owned();
    blocking(stop, move |stop| snapshot_at(&root, &id, &stop)).await
}

pub(super) fn snapshot_at(
    root: &Path,
    id: &str,
    stop: &CancellationToken,
) -> Result<NativeSnapshot, AgentError> {
    use rusqlite::OpenFlags;
    let path = root.join(id).join("store.db");
    let stat = std::fs::symlink_metadata(&path).map_err(|_| unavailable())?;
    if !stat.is_file() {
        return Err(unavailable());
    }
    let db = rusqlite::Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| unavailable())?;
    // `meta['0']` is hex-encoded JSON naming the current root blob.
    let meta: String = db
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .map_err(|_| unavailable())?;
    let meta: Value = hex::decode(meta.trim())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(unavailable)?;
    let root_id = meta["latestRootBlobId"].as_str().ok_or_else(unavailable)?;
    let mut blob = db
        .prepare("SELECT data FROM blobs WHERE id = ?1")
        .map_err(|_| unavailable())?;
    let mut read = |id: &str| -> Result<Vec<u8>, AgentError> {
        blob.query_row([id], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|_| unavailable())
    };
    // Field 1 of the root blob repeats the 32-byte ids of the conversation's
    // messages, each a JSON `{role, content}`.
    let root_blob = read(root_id)?;
    let message_ids = repeated_bytes(&root_blob, 1).ok_or_else(unavailable)?;
    if message_ids.len() > MAX_MESSAGES {
        return Err(unavailable());
    }
    let mut snapshot = NativeSnapshot::empty(id);
    let mut turn: Option<(String, Vec<Value>, Vec<String>)> = None;
    for message_id in message_ids {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        let message: Value =
            serde_json::from_slice(&read(&hex::encode(message_id))?).map_err(|_| unavailable())?;
        let role = message["role"].as_str().unwrap_or_default().to_owned();
        if role == "user" {
            if let Some(query) = user_query(&message["content"]) {
                if let Some(done) = turn.take() {
                    snapshot.entries.push(entry(done));
                }
                snapshot.last_user = Some(query.clone());
                snapshot.last_assistant = None;
                turn = Some((query, Vec::new(), vec![role]));
                continue;
            }
        }
        // The system prompt and injected context come before the first prompt.
        let Some((_, replies, roles)) = turn.as_mut() else {
            continue;
        };
        roles.push(role.clone());
        if role == "assistant" {
            let text = text_parts(&message["content"]);
            if !text.is_empty() {
                snapshot.last_assistant = Some(text.clone());
                replies.push(json!(text));
            }
        }
    }
    if let Some(done) = turn {
        snapshot.entries.push(entry(done));
    }
    Ok(snapshot)
}

fn entry((user, replies, roles): (String, Vec<Value>, Vec<String>)) -> Value {
    json!({ "user": user, "replies": replies, "roles": roles })
}

/// The prompt as sent, from the CLI's `<user_query>` wrapper.
fn user_query(content: &Value) -> Option<String> {
    let text = text_parts(content);
    let start = text.find(QUERY_OPEN)? + QUERY_OPEN.len();
    let end = text.rfind(QUERY_CLOSE)?;
    (end >= start).then(|| text[start..end].to_owned())
}

fn text_parts(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["type"] == "text")
            .filter_map(|part| part["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// Every value of one length-delimited protobuf field, skipping the others.
fn repeated_bytes(mut bytes: &[u8], wanted: u64) -> Option<Vec<&[u8]>> {
    let mut found = Vec::new();
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let (number, wire) = (key >> 3, key & 7);
        match wire {
            0 => {
                varint(&mut bytes)?;
            }
            1 => {
                take(&mut bytes, 8)?;
            }
            2 => {
                let len = usize::try_from(varint(&mut bytes)?).ok()?;
                let value = take(&mut bytes, len)?;
                if number == wanted {
                    found.push(value);
                }
            }
            5 => {
                take(&mut bytes, 4)?;
            }
            _ => return None,
        }
    }
    Some(found)
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    if bytes.len() < len {
        return None;
    }
    let (head, tail) = bytes.split_at(len);
    *bytes = tail;
    Some(head)
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, tail) = bytes.split_first()?;
        *bytes = tail;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}
