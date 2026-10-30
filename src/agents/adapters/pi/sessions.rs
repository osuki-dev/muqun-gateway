//! Read-only view of Pi's session files (`~/.pi/agent/sessions`).
//!
//! Pi owns these files and appends to them while a child runs, so the gateway
//! only reads them -- listing, titles and timelines come from here without a
//! child process -- and deletes one on request. Every entry is kept as a raw
//! `serde_json::Value`: Pi adds entry types between releases, and an unknown
//! one must not make a whole session unreadable.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

use crate::agents::adapters::t3::client::ms_from_iso;
use crate::agents::domain::{
    AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, ModelRef,
};
use crate::agents::ports::AgentError;

/// Longest title taken from the first user message, in characters.
const TITLE_CHARS: usize = 80;
const UNTITLED: &str = "New session";
pub(super) const MAX_SESSION_BYTES: usize = 16 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_SESSION_FILES: usize = 4096;
const MAX_SESSION_DIRS: usize = 1024;
const MAX_SUMMARY_SCAN_BYTES: usize = 128 * 1024 * 1024;
const MAX_SUMMARY_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_SUMMARY_ENTRIES: usize = 32_768;

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    summaries: Arc<Mutex<HashMap<PathBuf, CachedSummary>>>,
}

#[derive(Debug, Clone)]
struct CachedSummary {
    modified: Option<std::time::SystemTime>,
    bytes: u64,
    info: AgentSessionInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    pub cwd: String,
    pub created_ms: u64,
    pub modified_ms: u64,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            summaries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    /// `$HOME/.pi/agent/sessions`, where Pi writes unless told otherwise.
    pub fn default_root() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".pi").join("agent").join("sessions"))
    }

    /// Pi's per-cwd directory name: the leading separator is dropped and
    /// `/`, `\` and `:` become `-`, wrapped in `--`.
    pub fn dir_for_cwd(cwd: &str) -> String {
        let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
        let body: String = trimmed
            .chars()
            .map(|c| {
                if matches!(c, '/' | '\\' | ':') {
                    '-'
                } else {
                    c
                }
            })
            .collect();
        format!("--{body}--")
    }

    /// Session files, newest first. With `cwd`, only that directory's bucket is
    /// scanned; the header `cwd` is still what each file reports and is
    /// matched exactly, because the directory name is lossy (`/a-b` and `/a/b`
    /// collide).
    pub async fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionFile>, AgentError> {
        // The root itself too: with `--session-dir`,
        // `PI_CODING_AGENT_SESSION_DIR` or the `sessionDir` setting Pi writes
        // files flat into the directory instead of per-cwd buckets.
        let mut dirs = vec![self.root.clone()];
        match cwd {
            Some(cwd) => dirs.push(self.root.join(Self::dir_for_cwd(cwd))),
            None => {
                let Some(mut rd) = read_dir_or_none(&self.root).await? else {
                    return Ok(Vec::new());
                };
                while let Some(e) = rd.next_entry().await.map_err(io_err)? {
                    if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                        dirs.push(e.path());
                        if dirs.len() > MAX_SESSION_DIRS {
                            return Err(AgentError::Protocol(
                                "pi session directory limit exceeded".into(),
                            ));
                        }
                    }
                }
            }
        }
        let mut out = Vec::new();
        let mut scanned = 0;
        for dir in dirs {
            let Some(mut rd) = read_dir_or_none(&dir).await? else {
                continue;
            };
            while let Some(e) = rd.next_entry().await.map_err(io_err)? {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                scanned += 1;
                if scanned > MAX_SESSION_FILES {
                    return Err(AgentError::Protocol(
                        "pi session file limit exceeded".into(),
                    ));
                }
                // A file that vanished or is half-written is skipped, not fatal:
                // Pi may be creating or removing it while we scan.
                if let Some(file) = open_file(&path).await {
                    if cwd.is_none_or(|c| c == file.cwd) {
                        out.push(file);
                    }
                }
            }
        }
        out.sort_by(|a, b| {
            b.created_ms
                .cmp(&a.created_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub async fn find(&self, id: &str) -> Result<SessionFile, AgentError> {
        self.list(None)
            .await?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| AgentError::SessionNotFound(id.to_string()))
    }

    /// Every JSONL line as a `Value`, header first. Lines that do not parse
    /// are dropped: Pi may be mid-append on the last one.
    pub async fn read_entries(&self, file: &SessionFile) -> Result<Vec<Value>, AgentError> {
        let f = open_regular(&file.path)
            .await
            .map_err(|e| not_found_or_io(e, &file.id))?;
        let mut bytes = Vec::new();
        f.take((MAX_SESSION_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(io_err)?;
        if bytes.len() > MAX_SESSION_BYTES {
            return Err(AgentError::Protocol("pi session exceeds byte limit".into()));
        }
        let mut out = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            if line.len() > super::rpc::MAX_LINE_BYTES {
                return Err(AgentError::Protocol(
                    "pi session entry exceeds record limit".into(),
                ));
            }
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Listings need identifiers, titles and model changes, not image data
    /// or tool output. Project each bounded record while scanning so a large
    /// saved history cannot make the whole provider's session list fail.
    pub async fn read_summary(
        &self,
        file: &SessionFile,
        status: AgentSessionStatus,
    ) -> Result<AgentSessionInfo, AgentError> {
        let input = open_regular(&file.path)
            .await
            .map_err(|e| not_found_or_io(e, &file.id))?;
        let metadata = input.metadata().await.map_err(io_err)?;
        let file_bytes = metadata.len();
        let modified = metadata.modified().ok();
        if let Some(mut info) = self.summaries.lock().ok().and_then(|cache| {
            cache
                .get(&file.path)
                .filter(|entry| entry.modified == modified && entry.bytes == file_bytes)
                .map(|entry| entry.info.clone())
        }) {
            info.status = status;
            return Ok(info);
        }
        let mut reader = BufReader::new(input.take((MAX_SUMMARY_SCAN_BYTES + 1) as u64));
        let mut line = Vec::new();
        let mut entries = Vec::new();
        let mut retained = 0usize;
        let mut incomplete = file_bytes > MAX_SUMMARY_SCAN_BYTES as u64;
        loop {
            match super::rpc::read_line(&mut reader, &mut line, MAX_SUMMARY_RECORD_BYTES)
                .await
                .map_err(io_err)?
            {
                super::rpc::Line::Eof => break,
                super::rpc::Line::TooLong => {
                    incomplete = true;
                    // Consume the rest without retaining it; otherwise a
                    // suffix could be mistaken for another JSONL entry.
                    loop {
                        let available = reader.fill_buf().await.map_err(io_err)?;
                        if available.is_empty() {
                            break;
                        }
                        let end = available.iter().position(|b| *b == b'\n');
                        let used = end.map_or(available.len(), |n| n + 1);
                        reader.consume(used);
                        if end.is_some() {
                            break;
                        }
                    }
                    continue;
                }
                super::rpc::Line::Record => {}
            }
            if let Ok(entry) = serde_json::from_slice::<SummaryEntry>(&line) {
                let projected = entry.project();
                retained = retained.saturating_add(super::mapper::value_bytes(&projected));
                if retained > MAX_SESSION_BYTES {
                    incomplete = true;
                    break;
                }
                entries.push(projected);
            } else if !line.is_empty() {
                incomplete = true;
            }
            if entries.len() >= MAX_SUMMARY_ENTRIES {
                incomplete = true;
                break;
            }
        }
        let mut info = session_info(file, &entries, status);
        if incomplete {
            info.model = None;
            info.tokens = None;
            info.cost = None;
            info.error = Some(crate::agents::domain::AgentErrorInfo { name:"PiMetadataLimit".into(), message:"Pi session metadata could not be scanned completely; saved history remains unchanged".into(), status:None, retry_after_ms:None });
            if info.title == UNTITLED {
                info.title = "Pi session".into();
            }
        }
        if let Ok(mut cache) = self.summaries.lock() {
            if cache.len() >= 64 && !cache.contains_key(&file.path) {
                if let Some(path) = cache.keys().next().cloned() {
                    cache.remove(&path);
                }
            }
            cache.insert(
                file.path.clone(),
                CachedSummary {
                    modified,
                    bytes: file_bytes,
                    info: info.clone(),
                },
            );
        }
        Ok(info)
    }

    /// Remove the session file. Refuses anything not under the store root, so a
    /// forged `SessionFile` cannot turn this into an arbitrary file delete.
    pub async fn delete(&self, file: &SessionFile) -> Result<(), AgentError> {
        let root = tokio::fs::canonicalize(&self.root).await.map_err(io_err)?;
        let path = tokio::fs::canonicalize(&file.path)
            .await
            .map_err(|e| not_found_or_io(e, &file.id))?;
        if !path.starts_with(&root) || path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            return Err(AgentError::InvalidRequest(
                "session file is outside the Pi session directory".into(),
            ));
        }
        tokio::fs::remove_file(&path).await.map_err(io_err)
    }
}

#[derive(Deserialize)]
struct SummaryEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    fields: SummaryFields,
    #[serde(default)]
    message: Option<SummaryMessage>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryFields {
    id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<String>,
    provider: Option<String>,
    model_id: Option<String>,
    thinking_level: Option<String>,
    name: Option<String>,
    parent_session: Option<String>,
}

#[derive(Deserialize)]
struct SummaryMessage {
    role: String,
    #[serde(default, deserialize_with = "summary_content")]
    content: String,
    #[serde(default)]
    usage: Option<SummaryUsage>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryUsage {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    total_tokens: Option<u64>,
    cost: Option<SummaryCost>,
}

#[derive(Deserialize, serde::Serialize)]
struct SummaryCost {
    total: Option<f64>,
}

#[derive(Deserialize)]
struct SummaryBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, deserialize_with = "short_text")]
    text: String,
}

fn short_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct Text;
    impl serde::de::Visitor<'_> for Text {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("text")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.chars().take(TITLE_CHARS).collect())
        }
    }
    deserializer.deserialize_str(Text)
}

fn summary_content<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct Content;
    impl<'de> serde::de::Visitor<'de> for Content {
        type Value = String;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("message text or content blocks")
        }
        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
            Ok(value.chars().take(TITLE_CHARS).collect())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<String, A::Error> {
            let mut text = String::new();
            while let Some(block) = seq.next_element::<SummaryBlock>()? {
                if block.kind == "text" && text.chars().count() < TITLE_CHARS {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    let room = TITLE_CHARS.saturating_sub(text.chars().count());
                    text.extend(block.text.chars().take(room));
                }
            }
            Ok(text)
        }
    }
    deserializer.deserialize_any(Content)
}

impl SummaryEntry {
    fn project(self) -> Value {
        let mut entry = serde_json::to_value(self.fields).unwrap_or(Value::Null);
        entry["type"] = self.kind.into();
        if let Some(message) = self.message {
            entry["message"] = serde_json::json!({"role":message.role,"content":if message.role == "user" { message.content } else { String::new() },"usage":message.usage});
        }
        entry
    }
}

fn io_err(e: std::io::Error) -> AgentError {
    AgentError::RequestFailed(format!("pi session files: {e}"))
}

fn not_found_or_io(e: std::io::Error, id: &str) -> AgentError {
    if e.kind() == std::io::ErrorKind::NotFound {
        AgentError::SessionNotFound(id.to_string())
    } else {
        io_err(e)
    }
}

async fn read_dir_or_none(dir: &Path) -> Result<Option<tokio::fs::ReadDir>, AgentError> {
    match tokio::fs::read_dir(dir).await {
        Ok(rd) => Ok(Some(rd)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(e)),
    }
}

/// Build a `SessionFile` from the file name and the header line. `None` when
/// the name does not follow `<timestamp>_<id>.jsonl` or there is no header.
async fn open_file(path: &Path) -> Option<SessionFile> {
    let stem = path.file_stem()?.to_str()?;
    let (stamp, id) = stem.split_once('_')?;
    let meta = tokio::fs::metadata(path).await.ok()?;
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64);

    let f = open_regular(path).await.ok()?;
    let mut first = Vec::new();
    BufReader::new(f.take((MAX_HEADER_BYTES + 1) as u64))
        .read_until(b'\n', &mut first)
        .await
        .ok()?;
    if first.len() > MAX_HEADER_BYTES {
        return None;
    }
    let header: Value = serde_json::from_slice(&first).ok()?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return None;
    }
    if header.get("id").and_then(Value::as_str) != Some(id) {
        return None;
    }
    let cwd = header.get("cwd").and_then(Value::as_str)?.to_string();
    let created_ms = ms_from_file_stamp(stamp)
        .or_else(|| header.get("timestamp")?.as_str().and_then(ms_from_iso))
        .unwrap_or(modified_ms);
    Some(SessionFile {
        id: id.to_string(),
        path: path.to_path_buf(),
        cwd,
        created_ms,
        modified_ms,
    })
}

/// Open the final path without following symlinks or blocking on a FIFO.
/// Descriptor metadata closes the path-check/open race.
pub(super) async fn open_regular(path: &Path) -> std::io::Result<tokio::fs::File> {
    let path = path.to_owned();
    let file = tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Pi session is not a regular file",
            ));
        }
        Ok(file)
    })
    .await
    .map_err(|_| std::io::Error::other("Pi session opener failed"))??;
    Ok(tokio::fs::File::from_std(file))
}

/// `2026-10-14T06-51-18-225Z` -> epoch ms. Pi swaps the `:` and `.` of an ISO
/// timestamp for `-` so the name is legal on every filesystem.
fn ms_from_file_stamp(stamp: &str) -> Option<u64> {
    let (date, time) = stamp.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let mut p = time.splitn(3, '-');
    let (h, m, rest) = (p.next()?, p.next()?, p.next()?);
    let iso = match rest.split_once('-') {
        Some((s, ms)) => format!("{date}T{h}:{m}:{s}.{ms}Z"),
        None => format!("{date}T{h}:{m}:{rest}Z"),
    };
    ms_from_iso(&iso)
}

/// Entries on the path from the last entry back to the root, oldest first,
/// header excluded. The last line is the live leaf: Pi appends to whichever
/// branch is current, so after a fork the abandoned branch is simply not on
/// this path. A repeated id (corrupt file) ends the walk instead of looping.
pub fn active_branch(entries: &[Value]) -> Vec<Value> {
    let body: Vec<&Value> = entries
        .iter()
        .filter(|e| e.get("type").and_then(Value::as_str) != Some("session"))
        .collect();
    let Some(&last) = body.last() else {
        return Vec::new();
    };
    let by_id: HashMap<&str, &Value> = body
        .iter()
        .filter_map(|e| Some((e.get("id")?.as_str()?, *e)))
        .collect();
    let mut seen = HashSet::new();
    if let Some(id) = last.get("id").and_then(Value::as_str) {
        seen.insert(id);
    }
    let mut chain = vec![last];
    let mut cur = last;
    while let Some(pid) = cur.get("parentId").and_then(Value::as_str) {
        let Some(&parent) = by_id.get(pid) else { break };
        if !seen.insert(pid) {
            break;
        }
        chain.push(parent);
        cur = parent;
    }
    chain.into_iter().rev().cloned().collect()
}

/// Title: the latest `session_info` name, else the first user text, else a
/// placeholder.
fn title_of(branch: &[Value]) -> String {
    let named = branch
        .iter()
        .rev()
        .filter(|e| e.get("type").and_then(Value::as_str) == Some("session_info"))
        .filter_map(|e| e.get("name")?.as_str())
        .map(str::trim)
        .find(|n| !n.is_empty());
    if let Some(n) = named {
        return n.to_string();
    }
    let first_user = branch.iter().find_map(|e| {
        let m = e.get("message")?;
        if e.get("type")?.as_str()? != "message" || m.get("role")?.as_str()? != "user" {
            return None;
        }
        let text = match m.get("content")? {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text")?.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => return None,
        };
        let text = text.trim();
        (!text.is_empty()).then(|| text.chars().take(TITLE_CHARS).collect::<String>())
    });
    first_user.unwrap_or_else(|| UNTITLED.to_string())
}

fn model_of(branch: &[Value]) -> Option<ModelRef> {
    let last_of = |ty: &str| {
        branch
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(Value::as_str) == Some(ty))
    };
    let change = last_of("model_change")?;
    Some(ModelRef {
        provider_id: change.get("provider")?.as_str()?.to_string(),
        model_id: change.get("modelId")?.as_str()?.to_string(),
        variant: last_of("thinking_level_change")
            .and_then(|e| e.get("thinkingLevel")?.as_str())
            .map(str::to_string),
    })
}

/// Session summary from the file plus its entries. `agent_id` stays empty: the
/// manager stamps it, as for every adapter.
pub fn session_info(
    file: &SessionFile,
    entries: &[Value],
    status: AgentSessionStatus,
) -> AgentSessionInfo {
    let branch = active_branch(entries);
    let usage = super::mapper::usage_from_entries(&branch);
    AgentSessionInfo {
        asid: AgentSessionId(file.id.clone()),
        agent_id: String::new(),
        backend_session_id: String::new(),
        title: title_of(&branch),
        mode: None,
        model: model_of(&branch),
        status,
        runtime: None,
        context_usage: None,
        git_info: None,
        agent_name: None,
        agent_role: None,
        directory: Some(file.cwd.clone()),
        cost: usage.cost,
        tokens: (usage.total_tokens > 0).then_some(usage.tokens),
        limit: None,
        parent_id: None,
        project_id: None,
        outcome: None,
        error: None,
        revert: None,
        fork: entries
            .first()
            .and_then(|header| header.get("parentSession"))
            .and_then(Value::as_str)
            .and_then(|path| Path::new(path).file_stem())
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.split_once('_'))
            .and_then(|(_, id)| {
                uuid::Uuid::parse_str(id)
                    .ok()
                    .map(|_| crate::agents::domain::SessionForkInfo {
                        session_id: id.to_string(),
                        boundary_type: None,
                        message_id: None,
                    })
            }),
        time_idle: None,
        time_viewed: None,
        deleted: false,
        updated_ms: file.modified_ms,
    }
}

/// One project per distinct session `cwd`, sorted by path.
pub fn projects(files: &[SessionFile]) -> Vec<AgentProject> {
    let cwds: BTreeSet<&str> = files.iter().map(|f| f.cwd.as_str()).collect();
    cwds.into_iter()
        .map(|cwd| AgentProject {
            id: cwd.to_string(),
            canonical: cwd.to_string(),
            name: cwd
                .trim_end_matches(['/', '\\'])
                .rsplit(['/', '\\'])
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or(cwd)
                .to_string(),
            vcs: None,
            sandboxes: Vec::new(),
            missing: !Path::new(cwd).exists(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(dir: &Path, name: &str, lines: &[Value]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn header(id: &str, cwd: &str) -> Value {
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-10-14T06:51:18.225Z","cwd":cwd})
    }

    fn user(id: &str, parent: Option<&str>, text: &str) -> Value {
        json!({"type":"message","id":id,"parentId":parent,"timestamp":"2026-10-14T06:51:19.000Z",
               "message":{"role":"user","content":[{"type":"text","text":text}]}})
    }

    #[tokio::test]
    async fn store_lists_titles_and_follows_the_active_branch() {
        let root = std::env::temp_dir().join(format!("muqun-pi-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let demo = root.join(SessionStore::dir_for_cwd("/workspace/demo"));
        let other = root.join(SessionStore::dir_for_cwd("/workspace/other"));

        // Named, with a fork: a1 has two children (b1 abandoned, b2 live).
        let named = write(
            &demo,
            "2026-10-14T06-51-18-225Z_aaa.jsonl",
            &[
                header("aaa", "/workspace/demo"),
                user("a1", None, "first question"),
                json!({"type":"model_change","id":"b1","parentId":"a1","provider":"old","modelId":"gone"}),
                json!({"type":"model_change","id":"b2","parentId":"a1","provider":"openai","modelId":"gpt-x"}),
                json!({"type":"thinking_level_change","id":"c2","parentId":"b2","thinkingLevel":"high"}),
                json!({"type":"session_info","id":"d2","parentId":"c2","name":"Refactor auth"}),
            ],
        );
        write(
            &demo,
            "2026-10-15T01-00-00-000Z_bbb.jsonl",
            &[
                header("bbb", "/workspace/demo"),
                user("u1", None, &"x".repeat(200)),
            ],
        );
        write(
            &other,
            "2026-10-16T01-00-00-000Z_ccc.jsonl",
            &[header("ccc", "/workspace/other")],
        );

        let store = SessionStore::new(root.clone());
        assert_eq!(store.list(None).await.unwrap().len(), 3);
        let in_demo = store.list(Some("/workspace/demo")).await.unwrap();
        let ids: Vec<_> = in_demo.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["bbb", "aaa"], "filtered by cwd, newest first");
        assert_eq!(in_demo[1].created_ms, 1_791_960_678_225);

        // (session id, title, model): named / first user text capped / empty.
        let capped = "x".repeat(80);
        let cases = [
            (
                "aaa",
                "Refactor auth",
                Some(("openai", "gpt-x", Some("high"))),
            ),
            ("bbb", capped.as_str(), None),
            ("ccc", "New session", None),
        ];
        for (id, title, model) in cases {
            let f = store.find(id).await.unwrap();
            let entries = store.read_entries(&f).await.unwrap();
            let info = session_info(&f, &entries, AgentSessionStatus::Idle);
            assert_eq!(info.title, title, "{id}");
            let got = info.model.as_ref().map(|m| {
                (
                    m.provider_id.as_str(),
                    m.model_id.as_str(),
                    m.variant.as_deref(),
                )
            });
            assert_eq!(got, model, "{id}");
        }

        let f = store.find("aaa").await.unwrap();
        let entries = store.read_entries(&f).await.unwrap();
        let ids: Vec<_> = active_branch(&entries)
            .iter()
            .map(|e| e["id"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            ids,
            ["a1", "b2", "c2", "d2"],
            "abandoned fork b1 is excluded"
        );

        assert!(matches!(
            store.find("zzz").await,
            Err(AgentError::SessionNotFound(_))
        ));

        let outside =
            std::env::temp_dir().join(format!("muqun-pi-outside-{}.jsonl", std::process::id()));
        std::fs::write(&outside, "{}\n").unwrap();
        let forged = SessionFile {
            path: outside.clone(),
            ..f.clone()
        };
        assert!(store.delete(&forged).await.is_err());
        assert!(outside.exists());
        let _ = std::fs::remove_file(&outside);

        assert_eq!(f.path, named);
        store.delete(&f).await.unwrap();
        assert!(!named.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_for_cwd_and_projects() {
        for (cwd, dir) in [
            ("/workspace/demo", "--workspace-demo--"),
            ("C:\\work\\demo", "--C--work-demo--"),
        ] {
            assert_eq!(SessionStore::dir_for_cwd(cwd), dir);
        }
        let file = |cwd: &str| SessionFile {
            id: "i".into(),
            path: PathBuf::new(),
            cwd: cwd.into(),
            created_ms: 0,
            modified_ms: 0,
        };
        let p = projects(&[
            file("/workspace/demo"),
            file("/workspace/demo"),
            file("/workspace/gone"),
        ]);
        assert_eq!(p.len(), 2);
        assert_eq!(
            (p[0].id.as_str(), p[0].name.as_str()),
            ("/workspace/demo", "demo")
        );
    }

    #[tokio::test]
    async fn oversized_history_and_mismatched_identity_are_refused() {
        let root = std::env::temp_dir().join(format!("muqun-pi-bounds-{}", uuid::Uuid::new_v4()));
        let path = write(
            &root,
            "2026-10-14T06-51-18-225Z_large.jsonl",
            &[header("large", "/workspace/demo")],
        );
        let store = SessionStore::new(root.clone());
        let file = store.find("large").await.unwrap();
        let output = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        output.set_len((MAX_SESSION_BYTES + 1) as u64).unwrap();
        assert!(matches!(
            store.read_entries(&file).await,
            Err(AgentError::Protocol(_))
        ));
        write(
            &root,
            "2026-10-14T06-51-18-225Z_forged.jsonl",
            &[header("different", "/workspace/demo")],
        );
        assert!(matches!(
            store.find("forged").await,
            Err(AgentError::SessionNotFound(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fifo_and_symlink_sessions_are_refused_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join(format!("muqun-pi-regular-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let fifo = root.join("2026-10-14T06-51-18-225Z_fifo.jsonl");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path in this owned fixture.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), open_regular(&fifo))
                .await
                .unwrap()
                .is_err()
        );
        let outside = root.join("outside");
        std::fs::write(&outside, format!("{}\n", header("link", "/workspace/demo"))).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("2026-10-14T06-51-18-225Z_link.jsonl"))
            .unwrap();
        let store = SessionStore::new(root.clone());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), store.list(None))
                .await
                .unwrap()
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn large_saved_payloads_do_not_block_metadata_or_hide_history_errors() {
        let root = std::env::temp_dir().join(format!("muqun-pi-summary-{}", uuid::Uuid::new_v4()));
        let mut entries = vec![
            header("large", "/workspace/demo"),
            user("entry", None, "A real saved title"),
            json!({"type":"message","id":"usage","parentId":"entry","message":{"role":"assistant","content":[],"usage":{"input":10,"output":5,"totalTokens":15,"cost":{"total":0.01}}}}),
        ];
        for n in 0..6 {
            entries.push(json!({"type":"message", "id":format!("image-{n}"), "parentId":if n == 0 { "usage".to_string() } else { format!("image-{}",n-1) }, "timestamp":"2026-10-14T06:51:19.000Z", "message":{"role":"user", "content":[{"type":"image", "data":"x".repeat(3 * 1024 * 1024), "mimeType":"image/png"}]}}));
        }
        entries.push(json!({"type":"model_change", "id":"model", "parentId":"image-5", "provider":"example", "modelId":"chosen"}));
        entries.push(json!({"type":"thinking_level_change", "id":"effort", "parentId":"model", "thinkingLevel":"low"}));
        let path = write(&root, "2026-10-14T06-51-18-225Z_large.jsonl", &entries);
        let store = SessionStore::new(root.clone());
        let file = store.find("large").await.unwrap();
        assert!(std::fs::metadata(path).unwrap().len() > MAX_SESSION_BYTES as u64);
        let info = store
            .read_summary(&file, AgentSessionStatus::Idle)
            .await
            .unwrap();
        assert_eq!(info.title, "A real saved title");
        assert_eq!(info.model.unwrap().variant.as_deref(), Some("low"));
        assert!(info.error.is_none());
        assert_eq!(info.tokens.as_ref().unwrap().input, 10);
        assert_eq!(info.cost, Some(0.01));
        assert!(matches!(
            store.read_entries(&file).await,
            Err(AgentError::Protocol(_))
        ));
        let mut append = std::fs::OpenOptions::new()
            .append(true)
            .open(&file.path)
            .unwrap();
        use std::io::Write;
        writeln!(
            append,
            "{}",
            json!({"type":"session_info", "id":"name", "parentId":"effort", "name":"Updated title"})
        )
        .unwrap();
        let updated = store.find("large").await.unwrap();
        assert_eq!(
            store
                .read_summary(&updated, AgentSessionStatus::Idle)
                .await
                .unwrap()
                .title,
            "Updated title"
        );
        writeln!(append, "{}", json!({"type":"message","id":"too-large","parentId":"name","message":{"role":"user","content":[{"type":"image","data":"x".repeat(MAX_SUMMARY_RECORD_BYTES + 1)}]}})).unwrap();
        let incomplete = store
            .read_summary(
                &store.find("large").await.unwrap(),
                AgentSessionStatus::Idle,
            )
            .await
            .unwrap();
        assert!(incomplete.error.is_some());
        assert!(incomplete.model.is_none());
        assert!(
            incomplete.tokens.is_none(),
            "partial token totals must remain unknown"
        );
        assert!(
            incomplete.cost.is_none(),
            "partial costs must remain unknown"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    #[ignore = "read-only inspection of the installed Pi session metadata; no model calls"]
    async fn real_pi_session_summaries_are_readable_without_history_payloads() {
        let root = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .map(PathBuf::from)
            .or_else(SessionStore::default_root)
            .unwrap();
        let store = SessionStore::new(root);
        let files = store.list(None).await.unwrap();
        for file in &files {
            let info = store
                .read_summary(file, AgentSessionStatus::Idle)
                .await
                .unwrap();
            assert_eq!(info.asid.0, file.id);
            assert!(!info.title.is_empty());
        }
        eprintln!(
            "Pi native metadata summaries read successfully: {} sessions",
            files.len()
        );
    }
}
