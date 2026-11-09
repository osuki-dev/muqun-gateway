//! Where each agent's history is read from for the index, without waking a
//! session: every read here is a file or a native store on this machine, or
//! a pure read the agent's own adapter already serves from disk.
//!
//! | agent    | sessions                      | history                                    |
//! |----------|-------------------------------|--------------------------------------------|
//! | claude   | adapter list (transcripts)    | its transcript, through the ACP projection |
//! | codex    | adapter list (`thread/list`)  | the rollout file, appended bytes only      |
//! | opencode | its SQLite store, read-only   | `session_message` rows through its mapper  |
//! | pi       | adapter list (session files)  | adapter `get_timeline` (a file read)       |
//!
//! Every other agent's sessions are indexed from what reaches this gateway
//! live -- the domain events and the session mirror -- never by asking the
//! agent: its history read starts an ACP child (fx, qoder, cursor,
//! antigravity) or moves the App's own pagination cursors (deepseek, t3).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use super::index::{Mode, SessionMeta};
use crate::agents::domain::{
    text_item_id, tool_item_id, AgentPart, AgentSessionInfo, SessionQuery, TimelineItem,
    TimelineRole,
};
use crate::agents::ports::agent::AgentPort;

/// An agent whose history the index reads itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Claude,
    Codex,
    OpenCode,
    Pi,
}

impl Source {
    pub(crate) fn of(agent_id: &str) -> Option<Source> {
        Some(match agent_id {
            "claude" => Source::Claude,
            "codex" => Source::Codex,
            "opencode" => Source::OpenCode,
            "pi" => Source::Pi,
            _ => return None,
        })
    }
}

/// The sessions a source lists, each with the stamp of its current state.
pub(crate) struct Listing {
    pub sessions: Vec<(SessionMeta, String)>,
    /// The list is the whole set, so a session missing from it was deleted.
    pub complete: bool,
}

/// One read of a session's history.
pub(crate) struct Read {
    pub items: Vec<TimelineItem>,
    pub mode: Mode,
    /// Stored with the rows; equal to the listing's stamp once caught up.
    pub watermark: String,
}

pub(crate) fn meta(agent_id: &str, info: &AgentSessionInfo) -> SessionMeta {
    SessionMeta {
        agent_id: agent_id.to_owned(),
        asid: info.asid.0.clone(),
        title: info.title.clone(),
        directory: info.directory.clone(),
        parent_asid: info.parent_id.clone(),
        updated_ms: info.updated_ms,
    }
}

/// A source's sessions. `agent` is the attached adapter, asked only for
/// lists it serves without a process (and, for OpenCode, to prove its
/// store is the one this machine's service writes).
pub(crate) async fn list(source: Source, agent: &Arc<dyn AgentPort>) -> anyhow::Result<Listing> {
    match source {
        Source::OpenCode => {
            let listed = agent
                .list_sessions(&SessionQuery {
                    limit: Some(20),
                    ..SessionQuery::default()
                })
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let listing = tokio::task::spawn_blocking(opencode::list).await??;
            // An OpenCode on another host (an explicit URL) has its own store.
            anyhow::ensure!(
                listed.is_empty()
                    || listed
                        .iter()
                        .any(|s| listing.sessions.iter().any(|(m, _)| m.asid == s.asid.0)),
                "OpenCode's store on this machine is not the attached service's"
            );
            Ok(listing)
        }
        _ => {
            let infos = agent
                .list_sessions(&SessionQuery::default())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let agent_id = agent.kind();
            tokio::task::spawn_blocking(move || {
                let rollouts = (source == Source::Codex).then(codex::rollouts);
                let sessions = infos
                    .iter()
                    .filter_map(|info| {
                        let meta = meta(agent_id, info);
                        let stamp = match source {
                            Source::Claude => claude::stamp(&meta)?,
                            Source::Codex => codex::stamp(rollouts.as_ref()?.get(&meta.asid)?)?,
                            _ => meta.updated_ms.to_string(),
                        };
                        Some((meta, stamp))
                    })
                    .collect();
                Listing {
                    sessions,
                    // Claude's listing stops at 4,096 transcripts.
                    complete: source != Source::Claude || infos.len() < 4096,
                }
            })
            .await
            .map_err(Into::into)
        }
    }
}

/// The current stamp of one Claude or Codex session (`None` for the other
/// sources, which stamp only as they read).
pub(crate) async fn stamp(source: Source, meta: &SessionMeta) -> Option<String> {
    let meta = meta.clone();
    tokio::task::spawn_blocking(move || match source {
        Source::Claude => claude::stamp(&meta),
        Source::Codex => codex::stamp(codex::rollouts().get(&meta.asid)?),
        _ => None,
    })
    .await
    .ok()
    .flatten()
}

/// Read `meta`'s history after `watermark` (what the index already holds).
pub(crate) async fn read(
    source: Source,
    agent: &Arc<dyn AgentPort>,
    meta: &SessionMeta,
    watermark: Option<String>,
) -> anyhow::Result<Read> {
    match source {
        Source::Claude => {
            let directory = meta
                .directory
                .clone()
                .ok_or_else(|| anyhow::anyhow!("a Claude session without a directory"))?;
            let stamp = {
                let meta = meta.clone();
                tokio::task::spawn_blocking(move || claude::stamp(&meta))
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("transcript gone"))?
            };
            let stop = tokio_util::sync::CancellationToken::new();
            let mut items = crate::agents::adapters::claude::recovery::live_timeline(
                &directory, &meta.asid, &stop,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            // The replay stamps rows with the time it ran, not when they were
            // written; the session's own time is closer.
            for item in &mut items {
                item.updated_ms = 0;
            }
            Ok(Read {
                items,
                mode: Mode::Replace,
                watermark: stamp,
            })
        }
        Source::Codex => {
            let asid = meta.asid.clone();
            tokio::task::spawn_blocking(move || {
                let path = codex::rollouts()
                    .remove(&asid)
                    .ok_or_else(|| anyhow::anyhow!("rollout gone"))?;
                codex::read(&path, &asid, watermark.as_deref(), codex::STEP_BYTES)
            })
            .await?
        }
        Source::OpenCode => {
            let asid = meta.asid.clone();
            tokio::task::spawn_blocking(move || opencode::read(&asid, watermark.as_deref())).await?
        }
        Source::Pi => {
            let items = agent
                .get_timeline(&meta.asid, usize::MAX)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Read {
                items,
                mode: Mode::Replace,
                watermark: meta.updated_ms.to_string(),
            })
        }
    }
}

mod claude {
    use super::*;
    use crate::agents::adapters::claude::native_sessions::{project_dirs, projects_root};

    fn transcript(meta: &SessionMeta) -> Option<PathBuf> {
        let root = projects_root()?;
        let directory = meta.directory.as_deref()?;
        project_dirs(&root, directory)
            .into_iter()
            .map(|dir| dir.join(format!("{}.jsonl", meta.asid)))
            .find(|path| path.is_file())
    }

    /// The transcript's length and modification time: it changes with
    /// every record Claude Code appends or rewrites.
    pub(super) fn stamp(meta: &SessionMeta) -> Option<String> {
        let stat = std::fs::metadata(transcript(meta)?).ok()?;
        let modified = stat
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(format!("{}:{modified}", stat.len()))
    }
}

pub(crate) mod codex {
    use super::*;

    /// One read step: a 1.5 GB rollout is read across many steps, each
    /// committed with its offset, so a restart resumes and the build yields.
    pub(crate) const STEP_BYTES: u64 = 32 * 1024 * 1024;
    /// A rollout line longer than this (a huge tool output) is skipped
    /// without being held in memory.
    const MAX_LINE: usize = 4 * 1024 * 1024;

    fn home() -> Option<PathBuf> {
        match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
            Some(dir) => Some(PathBuf::from(dir)),
            None => Some(dirs::home_dir()?.join(".codex")),
        }
    }

    /// Each thread's rollout file, by thread id, from
    /// `sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`.
    pub(crate) fn rollouts() -> HashMap<String, PathBuf> {
        let mut out = HashMap::new();
        let Some(root) = home().map(|home| home.join("sessions")) else {
            return out;
        };
        walk(&root, 0, &mut out);
        out
    }

    fn walk(dir: &Path, depth: usize, out: &mut HashMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < 3 {
                walk(&path, depth + 1, out);
            } else if kind.is_file() {
                let name = entry.file_name();
                let Some(stem) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("rollout-"))
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                if let Some(thread) = stem.get(stem.len().saturating_sub(36)..) {
                    out.insert(thread.to_owned(), path);
                }
            }
        }
    }

    fn identity(stat: &std::fs::Metadata) -> u64 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            stat.ino()
        }
        #[cfg(not(unix))]
        {
            let _ = stat;
            0
        }
    }

    /// `<inode>:<length>`; the watermark reaches it once every byte is read.
    pub(crate) fn stamp(path: &Path) -> Option<String> {
        let stat = std::fs::metadata(path).ok()?;
        Some(format!("{}:{}", identity(&stat), stat.len()))
    }

    #[derive(Deserialize)]
    struct Line {
        #[serde(rename = "type")]
        kind: String,
        payload: Option<Payload>,
    }

    #[derive(Deserialize)]
    struct Payload {
        #[serde(rename = "type")]
        kind: Option<String>,
        turn_id: Option<String>,
        item: Option<Item>,
        started_at_ms: Option<u64>,
        completed_at_ms: Option<u64>,
    }

    #[derive(Deserialize)]
    struct Item {
        #[serde(rename = "type")]
        kind: String,
        id: Option<String>,
        content: Option<Vec<Content>>,
        command: Option<Vec<String>>,
        parsed_cmd: Option<Vec<Parsed>>,
        server: Option<String>,
        tool: Option<String>,
        changes: Option<HashMap<String, serde::de::IgnoredAny>>,
        query: Option<String>,
        path: Option<String>,
    }

    #[derive(Deserialize)]
    struct Content {
        text: Option<String>,
    }

    #[derive(Deserialize)]
    struct Parsed {
        cmd: Option<String>,
    }

    /// The rows the App shows for one completed item, keyed like the Codex
    /// adapter keys them (`mapper::message_id`), so a hit opens on its row.
    fn items(thread: &str, payload: Payload) -> Vec<TimelineItem> {
        let (Some(turn), Some(item)) = (payload.turn_id, payload.item) else {
            return Vec::new();
        };
        let Some(id) = item.id.clone() else {
            return Vec::new();
        };
        let message = crate::agents::adapters::codex::mapper::message_id(thread, &turn, &id);
        let at = payload
            .completed_at_ms
            .or(payload.started_at_ms)
            .unwrap_or(0);
        let text = || -> String {
            item.content
                .iter()
                .flatten()
                .filter_map(|c| c.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let tool = |name: &str, input: Value| -> Option<AgentPart> {
            serde_json::from_value(json!({
                "id": id, "name": name, "input": input,
                "state": "completed", "status": "completed",
            }))
            .ok()
            .map(AgentPart::Tool)
        };
        let (role, row, part) = match item.kind.as_str() {
            "UserMessage" => (
                TimelineRole::User,
                text_item_id(&message, 0),
                Some(AgentPart::Text { text: text() }),
            ),
            "AgentMessage" => (
                TimelineRole::Assistant,
                text_item_id(&message, 0),
                Some(AgentPart::Text { text: text() }),
            ),
            kind => {
                let part = match kind {
                    "CommandExecution" => {
                        let parsed: Vec<&str> = item
                            .parsed_cmd
                            .iter()
                            .flatten()
                            .filter_map(|p| p.cmd.as_deref())
                            .collect();
                        let command = if parsed.is_empty() {
                            item.command
                                .as_ref()
                                .and_then(|c| c.last().cloned())
                                .unwrap_or_default()
                        } else {
                            parsed.join(" && ")
                        };
                        tool("shell", json!({ "command": command }))
                    }
                    "FileChange" => {
                        let mut paths: Vec<&String> =
                            item.changes.iter().flat_map(|c| c.keys()).collect();
                        paths.sort();
                        tool("edit", json!({ "paths": paths }))
                    }
                    "McpToolCall" => tool(
                        &format!(
                            "{}:{}",
                            item.server.as_deref().unwrap_or_default(),
                            item.tool.as_deref().unwrap_or_default()
                        ),
                        json!({}),
                    ),
                    "Extension" | "WebSearch" if item.query.is_some() => {
                        tool("web_search", json!({ "query": item.query }))
                    }
                    "ImageView" => tool("view_image", json!({ "path": item.path })),
                    _ => None,
                };
                (TimelineRole::Assistant, tool_item_id(&message, &id), part)
            }
        };
        let Some(part) = part else {
            return Vec::new();
        };
        vec![TimelineItem {
            id: row,
            message_id: message,
            role,
            part,
            seq: 0,
            updated_ms: at,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        }]
    }

    /// One step of `path` from the watermark: at most `budget` bytes, ending
    /// on a line boundary (a line still being written waits for the next
    /// read). A new file (another inode) or a shorter one is read again
    /// from its start.
    pub(crate) fn read(
        path: &Path,
        thread: &str,
        watermark: Option<&str>,
        budget: u64,
    ) -> anyhow::Result<Read> {
        let mut file = std::fs::File::open(path)?;
        let stat = file.metadata()?;
        let inode = identity(&stat);
        let resume = watermark
            .and_then(|w| w.split_once(':'))
            .and_then(|(ino, off)| Some((ino.parse::<u64>().ok()?, off.parse::<u64>().ok()?)))
            .filter(|(ino, off)| *ino == inode && *off <= stat.len())
            .map(|(_, off)| off);
        let (mode, start) = match resume {
            Some(off) => (Mode::Merge, off),
            None => (Mode::Replace, 0),
        };
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut offset = start;
        let mut out = Vec::new();
        let mut line = Vec::new();
        // A line once started is read to its end, however long, so a step
        // always moves past it.
        while offset - start < budget {
            line.clear();
            let mut oversized = false;
            let mut ended = false;
            let mut consumed = 0u64;
            loop {
                let available = reader.fill_buf()?;
                if available.is_empty() {
                    break;
                }
                let end = memchr(available);
                let take = end.unwrap_or(available.len());
                if line.len() + take <= MAX_LINE {
                    line.extend_from_slice(&available[..take]);
                } else {
                    oversized = true;
                }
                let used = end.map_or(take, |at| at + 1);
                consumed += used as u64;
                reader.consume(used);
                if end.is_some() {
                    ended = true;
                    break;
                }
            }
            if !ended {
                // The file ends mid-line: that line is still being written.
                break;
            }
            offset += consumed;
            if oversized || !interesting(&line) {
                continue;
            }
            if let Ok(parsed) = serde_json::from_slice::<Line>(&line) {
                if parsed.kind == "event_msg" {
                    if let Some(payload) = parsed
                        .payload
                        .filter(|p| p.kind.as_deref() == Some("item_completed"))
                    {
                        out.extend(items(thread, payload));
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        if let Ok(file) = std::fs::File::open(path) {
            use std::os::fd::AsRawFd as _;
            // A one-off scan of gigabytes should not evict the owner's cache.
            unsafe {
                libc::posix_fadvise(
                    file.as_raw_fd(),
                    start as i64,
                    (offset - start) as i64,
                    libc::POSIX_FADV_DONTNEED,
                );
            }
        }
        Ok(Read {
            items: out,
            mode,
            watermark: format!("{inode}:{offset}"),
        })
    }

    fn memchr(bytes: &[u8]) -> Option<usize> {
        bytes.iter().position(|b| *b == b'\n')
    }

    /// Only completed items carry rows; their marker is near the line's
    /// start, so other lines (most of the bytes) are never parsed.
    fn interesting(line: &[u8]) -> bool {
        let head = &line[..line.len().min(256)];
        head.windows(16).any(|w| w == b"\"item_completed\"")
    }
}

mod opencode {
    use super::*;
    use rusqlite::{Connection, OpenFlags};

    fn store() -> Option<PathBuf> {
        let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()?.join(".local").join("share"),
        };
        Some(data.join("opencode").join("opencode.db"))
    }

    fn open() -> anyhow::Result<Connection> {
        let path = store().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
        let c = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        c.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(c)
    }

    /// Every session with `<time_updated>:<message count>` as its stamp.
    pub(super) fn list() -> anyhow::Result<Listing> {
        let c = open()?;
        let mut select = c.prepare(
            "SELECT s.id, coalesce(s.title, ''), s.directory, s.parent_id, s.time_updated,
                    (SELECT count(*) FROM session_message m WHERE m.session_id = s.id)
             FROM session_v2 s",
        )?;
        let sessions = select
            .query_map([], |r| {
                let updated: i64 = r.get(4)?;
                let count: i64 = r.get(5)?;
                Ok((
                    SessionMeta {
                        agent_id: "opencode".into(),
                        asid: r.get(0)?,
                        title: r.get(1)?,
                        directory: r.get(2)?,
                        parent_asid: r.get(3)?,
                        updated_ms: updated.max(0) as u64,
                    },
                    format!("{updated}:{count}"),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Listing {
            sessions,
            complete: true,
        })
    }

    /// The messages changed since the watermark, or all of them when any
    /// went (a revert) or nothing is held yet.
    pub(super) fn read(asid: &str, watermark: Option<&str>) -> anyhow::Result<Read> {
        let c = open()?;
        let (updated, count): (i64, i64) = c.query_row(
            "SELECT s.time_updated, (SELECT count(*) FROM session_message m WHERE m.session_id = s.id)
             FROM session_v2 s WHERE s.id = ?1",
            [asid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let held = watermark
            .and_then(|w| w.split_once(':'))
            .and_then(|(u, n)| Some((u.parse::<i64>().ok()?, n.parse::<i64>().ok()?)))
            .filter(|(_, n)| *n <= count);
        let (mode, since) = match held {
            Some((u, _)) => (Mode::Merge, u),
            None => (Mode::Replace, i64::MIN),
        };
        let mut select = c.prepare(
            "SELECT id, type, data FROM session_message
             WHERE session_id = ?1 AND time_updated >= ?2 ORDER BY seq",
        )?;
        let session = crate::agents::domain::AgentSessionId(asid.to_owned());
        let mut items = Vec::new();
        let rows = select.query_map(rusqlite::params![asid, since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, kind, data) = row?;
            let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            // Attachments are not searched and would be decoded for nothing.
            message.remove("files");
            message.remove("attachments");
            message.insert("id".into(), Value::String(id));
            message.insert("type".into(), Value::String(kind));
            items.extend(crate::agents::adapters::opencode::mapper::map_message(
                &Value::Object(message),
                &session,
            ));
        }
        Ok(Read {
            items,
            mode,
            watermark: format!("{updated}:{count}"),
        })
    }
}
