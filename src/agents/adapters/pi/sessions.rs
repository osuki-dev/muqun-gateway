//! Read-only view of Pi's session files (`~/.pi/agent/sessions`).
//!
//! Pi owns these files and appends to them while a child runs, so the gateway
//! only reads them -- listing, titles and timelines come from here without a
//! child process -- and deletes one on request. Every entry is kept as a raw
//! `serde_json::Value`: Pi adds entry types between releases, and an unknown
//! one must not make a whole session unreadable.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

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

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
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
        Self { root }
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
        let f = tokio::fs::File::open(&file.path)
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

    let f = tokio::fs::File::open(path).await.ok()?;
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
        fork: None,
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
}
