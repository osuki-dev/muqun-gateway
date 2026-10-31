//! List Claude Code sessions from its own transcript store without starting
//! the ACP adapter. Only the identity, workspace, modification time and title
//! of each transcript are read, from a bounded head and tail like Claude
//! Code's SDK does; request blobs and message bodies are never parsed.
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agents::ports::agent::AgentError;

/// The SDK reads this much of each end of a transcript for its listing.
const CHUNK: u64 = 64 * 1024;
/// The SDK cuts a longer encoded workspace and appends a runtime hash.
const MAX_ENCODED: usize = 200;
const MAX_PROJECTS: usize = 1024;
const MAX_ENTRIES: usize = 16_384;
const MAX_ROWS: usize = 4096;
const MAX_TITLE_CHARS: usize = 200;
const MAX_CACHED: usize = 8192;

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(dirs::home_dir()?.join(".claude")),
    }
}

/// `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects`.
pub(crate) fn projects_root() -> Option<PathBuf> {
    Some(config_dir()?.join("projects"))
}

const MAX_REGISTRY: usize = 512;
const MAX_REGISTRY_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registered {
    pid: u32,
    session_id: String,
    proc_start: Option<String>,
    status: Option<String>,
}

/// A Claude Code process outside this Gateway that has a session open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveProcess {
    pub pid: u32,
    /// Its start time, as `procStart` records it; with the pid it names
    /// this incarnation.
    pub start: String,
    /// Claude Code's own `busy` / `idle`, when it records one.
    pub busy: Option<bool>,
}

/// The sessions another live process has open, from Claude Code's own
/// registry: every running Claude Code (terminal, IDE or another ACP client)
/// keeps `<config>/sessions/<pid>.json` naming its current session.
/// `elsewhere(pid, proc_start)` says whether that pid is alive, is the same
/// process (start time), and is not one of this Gateway's own children.
fn live_in(
    registry: &Path,
    elsewhere: impl Fn(u32, Option<&str>) -> bool,
) -> HashMap<String, LiveProcess> {
    let Ok(entries) = std::fs::read_dir(registry) else {
        return HashMap::new();
    };
    entries
        .take(MAX_REGISTRY)
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| {
                name.strip_suffix(".json")
                    .is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
            }) && entry
                .metadata()
                .is_ok_and(|stat| stat.is_file() && stat.len() <= MAX_REGISTRY_BYTES)
        })
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Registered>(&bytes).ok())
        .filter(|entry| valid_session_id(&entry.session_id))
        .filter(|entry| elsewhere(entry.pid, entry.proc_start.as_deref()))
        .map(|entry| {
            let process = LiveProcess {
                pid: entry.pid,
                start: entry.proc_start.unwrap_or_default(),
                busy: match entry.status.as_deref() {
                    Some("busy") => Some(true),
                    Some("idle") => Some(false),
                    _ => None,
                },
            };
            (entry.session_id, process)
        })
        .collect()
}

/// `(ppid, start time)` from a Linux `/proc/<pid>/stat` line.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(stat: &str) -> Option<(u32, &str)> {
    // The command name may contain spaces and parentheses; fields follow the last ')'.
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?))
}

/// Alive, the same incarnation the registry recorded (where the start time
/// can be read), and not this Gateway or one of its children.
fn running_elsewhere(pid: u32, proc_start: Option<&str>) -> bool {
    use crate::terminal::backend::{descends_from, process_parent};
    if process_parent(pid).is_none() {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_stat(&stat).map(|(_, start)| start.to_owned()));
        if proc_start.is_some_and(|expected| start.as_deref() != Some(expected)) {
            return false;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = proc_start;
    !descends_from(pid, std::process::id())
}

/// Claude Code sessions open in another running process, by session id.
pub(crate) fn live_sessions() -> HashMap<String, LiveProcess> {
    match config_dir() {
        Some(config) => live_in(&config.join("sessions"), running_elsewhere),
        None => HashMap::new(),
    }
}

fn encode(cwd: &str) -> String {
    // JavaScript replaces each UTF-16 unit that is not ASCII alphanumeric.
    cwd.encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => byte as char,
            _ => '-',
        })
        .collect()
}

/// Whether the project directory `name` holds the transcripts of `cwd`:
/// the exact encoding, or for a long workspace the cut prefix with any hash
/// suffix (Claude Code and its SDK hash differently). Names are never decoded.
pub(crate) fn project_dir_matches(name: &str, cwd: &str) -> bool {
    let encoded = encode(cwd);
    if encoded.len() <= MAX_ENCODED {
        return name == encoded;
    }
    name.strip_prefix(&encoded[..MAX_ENCODED])
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|hash| !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub(crate) fn valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// The project directories under `root` that belong to `cwd`.
pub(crate) fn project_dirs(root: &Path, cwd: &str) -> Vec<PathBuf> {
    let encoded = encode(cwd);
    if encoded.len() <= MAX_ENCODED {
        let dir = root.join(&encoded);
        return if is_real_dir(&dir) {
            vec![dir]
        } else {
            Vec::new()
        };
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .take(MAX_PROJECTS)
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| project_dir_matches(name, cwd))
        })
        .map(|entry| entry.path())
        .filter(|dir| is_real_dir(dir))
        .collect()
}

fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|stat| stat.is_dir())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Summary {
    cwd: Option<String>,
    title: Option<String>,
    sidechain: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    inode: (u64, u64),
}

fn fingerprint(stat: &std::fs::Metadata) -> Fingerprint {
    Fingerprint {
        len: stat.len(),
        modified: stat.modified().ok(),
        #[cfg(unix)]
        inode: {
            use std::os::unix::fs::MetadataExt as _;
            (stat.dev(), stat.ino())
        },
    }
}

type SummaryCache = Mutex<HashMap<PathBuf, (Fingerprint, Option<Summary>)>>;

/// Transcripts already summarized, by path; a changed file is read again.
fn cache() -> &'static SummaryCache {
    static CACHE: OnceLock<SummaryCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub(super) async fn list(
    directory: Option<String>,
    stop: CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    let Some(root) = projects_root() else {
        return Ok(Vec::new());
    };
    tokio::task::spawn_blocking(move || list_in(&root, directory.as_deref(), &stop))
        .await
        .map_err(|_| AgentError::NotAvailable("Claude Code session reader failed".into()))?
}

fn list_in(
    root: &Path,
    directory: Option<&str>,
    stop: &CancellationToken,
) -> Result<Vec<Value>, AgentError> {
    let cancelled = || {
        stop.is_cancelled()
            .then(|| AgentError::NotAvailable("Claude Code runtime stopped".into()))
    };
    if !is_real_dir(root) {
        return Ok(Vec::new());
    }
    let dirs = match directory {
        Some(cwd) if Path::new(cwd).is_absolute() => project_dirs(root, cwd),
        Some(_) => Vec::new(),
        None => std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .take(MAX_PROJECTS)
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|dir| is_real_dir(dir))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut rows: Vec<(u64, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut entries = 0;
    for dir in dirs {
        if let Some(error) = cancelled() {
            return Err(error);
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.filter_map(Result::ok) {
            entries += 1;
            if entries > MAX_ENTRIES {
                break;
            }
            let name = file.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_suffix(".jsonl"))
                .filter(|id| valid_session_id(id))
            else {
                continue;
            };
            let path = file.path();
            let Ok(stat) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !stat.is_file() || !seen.insert(id.to_owned()) {
                continue;
            }
            let Some(summary) = summarize(&path, &stat) else {
                continue;
            };
            let cwd = match (&summary.cwd, directory) {
                // A lossy bucket can hold another workspace's transcripts.
                (Some(cwd), Some(wanted)) if cwd != wanted => continue,
                (Some(cwd), _) => cwd.clone(),
                (None, Some(wanted)) => wanted.to_owned(),
                (None, None) => continue,
            };
            let updated = stat
                .modified()
                .ok()
                .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |at| at.as_millis() as u64);
            rows.push((
                updated,
                json!({
                    "sessionId": id,
                    "cwd": cwd,
                    "title": summary.title,
                    "updatedAt": crate::agents::adapters::t3::client::iso_from_ms(updated),
                }),
            ));
        }
        if entries > MAX_ENTRIES {
            break;
        }
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    rows.truncate(MAX_ROWS);
    Ok(rows.into_iter().map(|(_, row)| row).collect())
}

/// A listable transcript's summary, or `None` for a sidechain or one with
/// no title or prompt (the SDK hides those too).
fn summarize(path: &Path, stat: &std::fs::Metadata) -> Option<Summary> {
    let print = fingerprint(stat);
    if let Some((known, summary)) = cache().lock().unwrap().get(path) {
        if *known == print {
            return summary.clone();
        }
    }
    let summary = read_summary(path, stat.len()).filter(|s| !s.sidechain && s.title.is_some());
    let mut cache = cache().lock().unwrap();
    if cache.len() >= MAX_CACHED {
        cache.clear();
    }
    cache.insert(path.to_owned(), (print, summary.clone()));
    summary
}

fn read_summary(path: &Path, len: u64) -> Option<Summary> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let mut head = Vec::new();
    (&mut file).take(CHUNK).read_to_end(&mut head).ok()?;
    let whole = head.len() as u64 >= len;
    let tail = if whole {
        Vec::new()
    } else {
        file.seek(SeekFrom::Start(len.saturating_sub(CHUNK))).ok()?;
        let mut tail = Vec::new();
        (&mut file).take(CHUNK).read_to_end(&mut tail).ok()?;
        tail
    };
    let head = Fields::scan(complete_lines(&head, false, !whole));
    let tail = Fields::scan(complete_lines(&tail, true, false));
    let title = tail
        .custom_title
        .or(head.custom_title)
        .or(tail.ai_title)
        .or(head.ai_title)
        .or(tail.last_prompt)
        .or(tail.summary)
        .or(head.last_prompt)
        .or(head.first_prompt);
    Some(Summary {
        cwd: head.cwd,
        title: title.map(|title| clean_title(&title)),
        sidechain: head.sidechain,
    })
}

/// The complete lines of a bounded chunk: a tail starts mid-line and a
/// truncated head ends mid-line; neither partial row is accepted.
fn complete_lines(bytes: &[u8], skip_first: bool, drop_last: bool) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    if skip_first && !lines.is_empty() {
        lines.remove(0);
    }
    if (drop_last || !bytes.ends_with(b"\n")) && !lines.is_empty() {
        lines.pop();
    }
    lines.retain(|line| !line.is_empty());
    lines
}

#[derive(Default)]
struct Fields {
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    summary: Option<String>,
    first_prompt: Option<String>,
    cwd: Option<String>,
    sidechain: bool,
    chat_seen: bool,
}

/// The few fields a listing needs from any record; the rest is skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type")]
    kind: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    last_prompt: Option<String>,
    summary: Option<String>,
    uuid: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    #[serde(default)]
    is_meta: bool,
    message: Option<ChatMessage>,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: Option<Value>,
}

/// The `type` of a record whose first key it is, as Claude Code writes its
/// metadata records (titles, request blobs, cost, queue).
pub(crate) fn metadata_type(line: &[u8]) -> Option<&str> {
    let rest = line.strip_prefix(br#"{"type":""#)?;
    let end = rest.iter().position(|byte| *byte == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

const TITLE_TYPES: [&str; 4] = ["custom-title", "ai-title", "last-prompt", "summary"];

impl Fields {
    fn scan(lines: Vec<&[u8]>) -> Self {
        let mut fields = Self::default();
        for line in lines {
            // Request blobs and other bulky metadata are skipped unparsed.
            if metadata_type(line).is_some_and(|kind| !TITLE_TYPES.contains(&kind)) {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Record>(line) else {
                continue;
            };
            if record
                .kind
                .as_deref()
                .is_some_and(|kind| TITLE_TYPES.contains(&kind))
            {
                let keep = |slot: &mut Option<String>, value: Option<String>| {
                    if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
                        *slot = Some(value);
                    }
                };
                keep(&mut fields.custom_title, record.custom_title);
                keep(&mut fields.ai_title, record.ai_title);
                keep(&mut fields.last_prompt, record.last_prompt);
                keep(&mut fields.summary, record.summary);
                continue;
            }
            if record.uuid.is_none() && record.cwd.is_none() {
                continue;
            }
            if !fields.chat_seen {
                fields.chat_seen = true;
                fields.sidechain = record.is_sidechain;
            }
            if fields.cwd.is_none() {
                fields.cwd = record
                    .cwd
                    .filter(|cwd| Path::new(cwd).is_absolute() && cwd.len() <= 8192);
            }
            if fields.first_prompt.is_none()
                && record.kind.as_deref() == Some("user")
                && !record.is_meta
            {
                fields.first_prompt = record
                    .message
                    .and_then(|message| message.content)
                    .and_then(|content| prompt_text(&content));
            }
        }
        fields
    }
}

/// The text of a typed prompt; tool results and command wrappers are not one.
fn prompt_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            if parts.iter().any(|part| part["type"] == "tool_result") {
                return None;
            }
            parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty() && !text.starts_with('<')).then(|| text.to_owned())
}

fn clean_title(title: &str) -> String {
    let mut clean = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((index, _)) = clean.char_indices().nth(MAX_TITLE_CHARS) {
        clean.truncate(index);
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registry_marks_sessions_live_only_in_another_running_process() {
        let dir = Scratch(
            std::env::temp_dir().join(format!("muqun-claude-registry-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir_all(&dir.0).unwrap();
        let ids = [
            "00000000-0000-4000-8000-00000000000a",
            "00000000-0000-4000-8000-00000000000b",
            "00000000-0000-4000-8000-00000000000c",
        ];
        // (pid, session, file name)
        for (pid, id, name) in [
            (11, ids[0], "11.json"), // a terminal elsewhere: live
            (12, ids[1], "12.json"), // exited, or one of ours: not live
            (13, ids[2], "13.key"),  // not a registry entry
            (14, "not-a-session", "14.json"),
        ] {
            let entry =
                json!({"pid": pid, "sessionId": id, "procStart": "100", "kind": "interactive"});
            std::fs::write(dir.0.join(name), entry.to_string()).unwrap();
        }
        let live = live_in(&dir.0, |pid, start| pid != 12 && start == Some("100"));
        assert_eq!(live.keys().collect::<Vec<_>>(), [ids[0]]);
        assert_eq!(live[ids[0]].pid, 11);
        assert!(live_in(&dir.0.join("missing"), |_, _| true).is_empty());
        for (stat, parsed) in [
            (
                "42 (claude) S 7 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 15811020 1",
                Some((7, "15811020")),
            ),
            (
                "42 (a) b) (c) S 9 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 77 1",
                Some((9, "77")),
            ),
            ("42 (short) S", None),
        ] {
            assert_eq!(parse_stat(stat), parsed, "{stat}");
        }
    }

    #[test]
    fn workspace_buckets_follow_the_sdk_encoding_and_never_decode() {
        for (cwd, name, matches) in [
            ("/home/u/my.app", "-home-u-my-app", true),
            ("/home/u/my_app", "-home-u-my-app", true),
            ("C:\\work\\x", "C--work-x", true),
            ("/home/u/项目", "-home-u---", true),
            ("/home/u/app", "-home-u-app-2", false),
        ] {
            assert_eq!(project_dir_matches(name, cwd), matches, "{cwd}");
        }
        let long = format!("/{}", "a".repeat(250));
        let cut = &encode(&long)[..MAX_ENCODED];
        assert!(project_dir_matches(&format!("{cut}-1x9z"), &long));
        assert!(!project_dir_matches(cut, &long));
        assert!(!project_dir_matches(&format!("{cut}x-1x9z"), &long));
    }

    #[test]
    #[cfg_attr(windows, ignore = "not yet ported to Windows; see docs/windows.md")]
    fn listing_reads_title_and_workspace_and_skips_blob_records() {
        let root = Scratch(
            std::env::temp_dir().join(format!("muqun-claude-list-{}", uuid::Uuid::new_v4())),
        );
        let cwd = "/synthetic/workspace";
        let bucket = root.0.join(encode(cwd));
        std::fs::create_dir_all(&bucket).unwrap();
        let id = "00000000-0000-4000-8000-000000000001";
        // A request blob that is not JSON at all proves it is never parsed.
        let blob = format!(
            "{{\"type\":\"api-request-blob\",\"message\":\"{}",
            "x".repeat(200_000)
        );
        let content = format!(
            "{}\n{blob}\n{}\n",
            json!({"parentUuid":null,"isSidechain":false,"type":"user","cwd":cwd,
                   "message":{"role":"user","content":"First synthetic prompt"}}),
            json!({"type":"ai-title","aiTitle":"Synthetic title","sessionId":id}),
        );
        std::fs::write(bucket.join(format!("{id}.jsonl")), content).unwrap();
        // Not a session id: ignored.
        std::fs::write(bucket.join("notes.jsonl"), "{}\n").unwrap();
        let stop = CancellationToken::new();
        let rows = list_in(&root.0, Some(cwd), &stop).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["sessionId"], id);
        assert_eq!(rows[0]["cwd"], cwd);
        assert_eq!(rows[0]["title"], "Synthetic title");
        assert!(rows[0]["updatedAt"].as_str().unwrap().ends_with('Z'));
        assert_eq!(list_in(&root.0, None, &stop).unwrap(), rows);
        assert!(list_in(&root.0, Some("/synthetic/other"), &stop)
            .unwrap()
            .is_empty());
    }
}
