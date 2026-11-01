//! Read-only views of Antigravity's own store under `~/.gemini/antigravity-cli`:
//! the conversation index (`conversation_summaries.db`) for listing, and one
//! conversation's steps (`conversations/<id>.db`) as retirement evidence.
//! Nothing here starts `agy` or writes to its files.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::ports::agent::AgentError;

const MAX_ROWS: usize = 4096;
const MAX_STEPS: usize = 100_000;
const MAX_TITLE_CHARS: usize = 200;
/// `agy`'s step kinds: the user's input and the model's reply.
const USER_INPUT: i64 = 14;
const PLANNER_RESPONSE: i64 = 15;

fn unavailable() -> AgentError {
    AgentError::NotAvailable("Antigravity's conversation store is unreadable".into())
}

fn root() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".gemini").join("antigravity-cli"))
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

fn open(path: &Path) -> Result<rusqlite::Connection, AgentError> {
    use rusqlite::OpenFlags;
    rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| unavailable())
}

/// Top-level conversations as ACP `session/list` rows, newest first. Subagent
/// conversations, never-prompted ones and ones without a folder are left out.
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
    let path = root.join("conversation_summaries.db");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let db = open(&path)?;
    let mut query = db
        .prepare(
            "SELECT conversation_id, title, preview, workspace_uris,
                    max(strftime('%Y-%m-%dT%H:%M:%fZ', last_modified_time),
                        strftime('%Y-%m-%dT%H:%M:%fZ', last_user_input_time))
             FROM conversation_summaries
             WHERE nesting_depth = 0 AND parent_conversation_id = '' AND step_count > 0
             ORDER BY 5 DESC LIMIT ?1",
        )
        .map_err(|_| unavailable())?;
    let rows = query
        .query_map([MAX_ROWS as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|_| unavailable())?;
    let mut sessions = Vec::new();
    for row in rows {
        if stop.is_cancelled() {
            return Err(unavailable());
        }
        let (id, title, preview, workspaces, updated) = row.map_err(|_| unavailable())?;
        let Some(cwd) = first_folder(&workspaces) else {
            continue;
        };
        if !valid_id(&id) || directory.is_some_and(|wanted| wanted != cwd) {
            continue;
        }
        // A task's first prompt ends with the task sentence; a preview
        // title stops before it.
        let title = if title.trim().is_empty() {
            preview
                .split(crate::mcp::inject::TASK_SENTENCE)
                .next()
                .unwrap_or_default()
                .to_owned()
        } else {
            title
        };
        let title: String = title.trim().chars().take(MAX_TITLE_CHARS).collect();
        let mut session = json!({ "sessionId": id, "cwd": cwd });
        if !title.is_empty() {
            session["title"] = json!(title);
        }
        // `agy` writes year 1 for a time it never recorded.
        if let Some(updated) = updated.filter(|time| !time.starts_with("0001-")) {
            session["updatedAt"] = json!(updated);
        }
        sessions.push(session);
    }
    Ok(sessions)
}

/// The first `file://` workspace folder of a JSON array of URIs.
fn first_folder(workspaces: &str) -> Option<String> {
    let uris: Vec<String> = serde_json::from_str(workspaces).ok()?;
    let path = uris.first()?.strip_prefix("file://")?;
    let path = path.strip_prefix("localhost").unwrap_or(path);
    let path = percent_decode(path)?;
    Path::new(&path).is_absolute().then_some(path)
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .ok()
        .filter(|path| !path.contains('\0'))
}

/// One entry per user turn: its text, then each reply's text and every
/// step's kind. The last reply's text is what the live stream showed last.
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
    let path = root.join("conversations").join(format!("{id}.db"));
    let stat = std::fs::symlink_metadata(&path).map_err(|_| unavailable())?;
    if !stat.is_file() {
        return Err(unavailable());
    }
    let db = open(&path)?;
    let mut query = db
        .prepare("SELECT step_type, step_payload FROM steps ORDER BY idx LIMIT ?1")
        .map_err(|_| unavailable())?;
    let rows = query
        .query_map([MAX_STEPS as i64 + 1], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })
        .map_err(|_| unavailable())?;
    let mut snapshot = NativeSnapshot::empty(id);
    let mut turn: Option<(String, Vec<Value>, Vec<i64>)> = None;
    let mut count = 0;
    for row in rows {
        count += 1;
        if stop.is_cancelled() || count > MAX_STEPS {
            return Err(unavailable());
        }
        let (kind, payload) = row.map_err(|_| unavailable())?;
        let payload = payload.unwrap_or_default();
        if kind == USER_INPUT {
            let text = field_text(&payload, &[19, 2]).ok_or_else(unavailable)?;
            if let Some(done) = turn.take() {
                snapshot.entries.push(entry(done));
            }
            snapshot.last_user = Some(text.clone());
            snapshot.last_assistant = None;
            turn = Some((text, Vec::new(), vec![kind]));
            continue;
        }
        // Steps before the first user input are setup, not conversation.
        let Some((_, replies, kinds)) = turn.as_mut() else {
            continue;
        };
        kinds.push(kind);
        if kind == PLANNER_RESPONSE {
            let text = field_text(&payload, &[20, 1]).unwrap_or_default();
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

fn entry((user, replies, kinds): (String, Vec<Value>, Vec<i64>)) -> Value {
    json!({ "user": user, "replies": replies, "steps": kinds })
}

/// The UTF-8 string at a path of length-delimited protobuf fields, taking the
/// first occurrence at each level.
fn field_text(message: &[u8], path: &[u64]) -> Option<String> {
    let (&wanted, rest) = path.split_first()?;
    let mut bytes = message;
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let (number, wire) = (key >> 3, key & 7);
        let value = match wire {
            0 => {
                varint(&mut bytes)?;
                None
            }
            1 => {
                take(&mut bytes, 8)?;
                None
            }
            2 => {
                let len = usize::try_from(varint(&mut bytes)?).ok()?;
                Some(take(&mut bytes, len)?)
            }
            5 => {
                take(&mut bytes, 4)?;
                None
            }
            _ => return None,
        };
        if number == wanted {
            let value = value?;
            return if rest.is_empty() {
                String::from_utf8(value.to_vec()).ok()
            } else {
                field_text(value, rest)
            };
        }
    }
    None
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
