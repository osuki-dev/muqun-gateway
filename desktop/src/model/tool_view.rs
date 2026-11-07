//! What a tool call is, and what is worth showing of what it said.
//!
//! Pure readings of a [`ToolCall`], ported from the App's
//! `agent-tool-output.ts`, `muqun-tool-presentation.ts` and
//! `agent-permission-safety.ts`, so the desktop's cards classify, title and
//! parse a call the way the App does. Nothing here draws; `ui::parts` picks
//! a body from these.

use muqun_gateway::agents::{PermissionRequest, ToolCall, ToolCallStatus};
use serde_json::{Map, Value};

/// What the gateway keeps per call, and therefore the most a card is handed.
pub const OUTPUT_BYTE_CAP: usize = 64 * 1024;
/// How far [`parse_output`] unwraps before it shows what it has.
const OUTPUT_MAX_DEPTH: usize = 6;

/// The App's tool families (`ToolKind`), plus `Sleep` (its own header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolKind {
    Shell,
    Read,
    Edit,
    Write,
    Patch,
    Glob,
    Grep,
    Search,
    Web,
    Subagent,
    Skill,
    Todo,
    Question,
    Execute,
    Browser,
    Mcp,
}

/// App `classifyTool`, plus the names Claude Code, Codex and Cursor use.
pub fn classify(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "shell" | "bash" | "terminal" | "run" | "exec_command" | "local_shell" | "bashoutput"
        | "run_command" => ToolKind::Shell,
        "read" | "list" | "cat" | "ls" | "read_file" => ToolKind::Read,
        "edit" | "multiedit" | "str_replace" | "str_replace_editor" | "notebookedit"
        | "edit_file" => ToolKind::Edit,
        "write" | "create" | "new_file" | "write_file" => ToolKind::Write,
        "patch" | "apply_patch" => ToolKind::Patch,
        "glob" | "find" => ToolKind::Glob,
        "grep" | "ripgrep" => ToolKind::Grep,
        "search" | "codesearch" => ToolKind::Search,
        "webfetch" | "fetch" | "websearch" | "web_search" => ToolKind::Web,
        "subagent" | "task" | "spawn" | "agent" | "invoke_subagent" => ToolKind::Subagent,
        "skill" | "load_skill" => ToolKind::Skill,
        "todowrite" | "todoread" | "todo" | "tasks" | "updatetodos" | "todo_write" => {
            ToolKind::Todo
        }
        "question" | "ask" | "askuserquestion" => ToolKind::Question,
        "execute" | "code" => ToolKind::Execute,
        "browser" => ToolKind::Browser,
        _ => ToolKind::Mcp,
    }
}

fn pick<'a>(rec: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        rec.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
}

const PATH_KEYS: &[&str] = &[
    "path",
    "filePath",
    "file_path",
    "file",
    "TargetFile",
    "Path",
    "AbsolutePath",
];

/// The call's input as an object: the parsed input, `arguments` inside it
/// (MCP), or the string fields a still-streaming partial JSON has reached.
pub fn input_record(call: &ToolCall) -> Option<Map<String, Value>> {
    let direct = match &call.input {
        Value::Object(map) => Some(map),
        _ => None,
    };
    if let Some(map) = direct.filter(|m| !m.is_empty()) {
        if let Some(Value::Object(args)) = map.get("arguments") {
            return Some(args.clone());
        }
        return Some(map.clone());
    }
    let raw = match &call.input {
        Value::String(s) => Some(s.as_str()),
        _ => call.input_partial.as_deref(),
    }?;
    let trimmed = raw.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        return Some(map);
    }
    let partial = partial_json_strings(trimmed);
    (!partial.is_empty()).then_some(partial)
}

/// The string fields a half-written JSON object has got to so far (App
/// `partialJsonStrings`): every completed `"key": "value"`, plus the last
/// value even before its closing quote.
pub fn partial_json_strings(text: &str) -> Map<String, Value> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Map::new();
    let mut i = 0;
    let read = |i: &mut usize| -> (String, bool) {
        *i += 1;
        let mut value = String::new();
        while *i < chars.len() {
            let c = chars[*i];
            if c == '\\' {
                match chars.get(*i + 1) {
                    None => {
                        *i += 1;
                        return (value, false);
                    }
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some(&other) => value.push(other),
                }
                *i += 2;
                continue;
            }
            if c == '"' {
                *i += 1;
                return (value, true);
            }
            value.push(c);
            *i += 1;
        }
        (value, false)
    };
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        let (key, closed) = read(&mut i);
        if !closed {
            break;
        }
        while i < chars.len() && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        if chars.get(i) != Some(&':') {
            continue;
        }
        i += 1;
        while i < chars.len() && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] != '"' {
            match chars[i..].iter().position(|c| *c == ',') {
                Some(at) => i += at + 1,
                None => break,
            }
            continue;
        }
        let (value, closed) = read(&mut i);
        if !key.is_empty() {
            out.insert(key, Value::String(value));
        }
        if !closed {
            break;
        }
    }
    out
}

/// App `extractTarget`: the one line a call is pointed at.
pub fn target(kind: ToolKind, rec: Option<&Map<String, Value>>) -> String {
    let Some(rec) = rec else {
        return String::new();
    };
    let s = |keys: &[&str]| pick(rec, keys).unwrap_or_default().to_string();
    match kind {
        ToolKind::Shell => {
            if let Some(Value::Array(argv)) = rec.get("command") {
                return argv
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            s(&["command", "cmd", "CommandLine"])
        }
        ToolKind::Read | ToolKind::Edit | ToolKind::Write => pick(rec, PATH_KEYS)
            .or_else(|| {
                // Codex names an edit's files as a list.
                rec.get("paths")
                    .and_then(Value::as_array)
                    .and_then(|p| p.first())
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string(),
        ToolKind::Patch => pick(rec, &["patchText", "patch_text", "patch", "input"])
            .and_then(|t| parse_patch_sections(t).into_iter().next())
            .map(|p| p.path)
            .unwrap_or_else(|| s(PATH_KEYS)),
        ToolKind::Glob | ToolKind::Grep | ToolKind::Search => {
            s(&["pattern", "query", "literal", "glob_pattern"])
        }
        ToolKind::Web => s(&["url", "query"]),
        ToolKind::Subagent => pick(rec, &["description"])
            .or_else(|| pick(rec, &["agent", "agentID", "agent_id", "subagent_type"]))
            .map(str::to_string)
            .or_else(|| pick(rec, &["prompt"]).map(|p| first_line(p).to_string()))
            .unwrap_or_default(),
        ToolKind::Skill => s(&["id", "skillID", "skill_id", "skillId", "skill", "name"]),
        ToolKind::Question => parse_questions(rec)
            .into_iter()
            .next()
            .map(|q| {
                if q.header.is_empty() {
                    q.question
                } else {
                    q.header
                }
            })
            .unwrap_or_default(),
        ToolKind::Execute => first_line(pick(rec, &["code"]).unwrap_or_default()).to_string(),
        ToolKind::Browser => s(&["url", "action", "tabID"]),
        ToolKind::Todo | ToolKind::Mcp => s(&[
            "command",
            "cmd",
            "path",
            "filePath",
            "file_path",
            "file",
            "query",
            "url",
            "pattern",
            "name",
        ]),
    }
}

/// App `extractCaption`: the quieter second line.
pub fn caption(kind: ToolKind, rec: Option<&Map<String, Value>>) -> String {
    let Some(rec) = rec else {
        return String::new();
    };
    let s = |keys: &[&str]| pick(rec, keys).unwrap_or_default().to_string();
    match kind {
        ToolKind::Read | ToolKind::Edit | ToolKind::Write => dirname(&s(PATH_KEYS)).to_string(),
        ToolKind::Glob | ToolKind::Grep | ToolKind::Search => s(&[
            "path",
            "filePath",
            "file_path",
            "file",
            "include",
            "directory",
            "glob",
        ]),
        ToolKind::Shell => s(&["workdir", "cwd"]),
        ToolKind::Subagent => s(&["agent", "agentID", "agent_id", "subagent_type"]),
        ToolKind::Web if pick(rec, &["url"]).is_some() => String::new(),
        ToolKind::Web => String::new(),
        _ => String::new(),
    }
}

pub fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or(text)
}

/// The folder a path is in (`''` for a bare name, `/` at the root).
pub fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        None => "",
        Some(0) => "/",
        Some(at) => &path[..at],
    }
}

/// The last path segment.
pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
}

/// The code fence language for a file, when Rune's highlighter knows it.
pub fn fence_language(path: &str) -> Option<&'static str> {
    let name = basename(path);
    let dot = name.rfind('.').filter(|d| *d > 0)?;
    Some(match name[dot + 1..].to_ascii_lowercase().as_str() {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "json" | "jsonc" => "json",
        "html" | "htm" | "xml" | "svg" => "html",
        "css" | "scss" => "css",
        "yml" | "yaml" => "yaml",
        "go" => "go",
        "py" | "pyi" => "python",
        "rs" => "rust",
        "sh" | "bash" | "zsh" => "bash",
        "swift" => "swift",
        "sql" => "sql",
        _ => return None,
    })
}

/// Whatever a tool said, as text, with the exit line lifted out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub stdout: String,
    pub exit: Option<i64>,
    pub truncated: bool,
}

fn cap(text: &str) -> (String, bool) {
    if text.len() <= OUTPUT_BYTE_CAP {
        return (text.to_string(), false);
    }
    let mut end = OUTPUT_BYTE_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// `Command exited with code N` anywhere in `text`: (the line's span, N).
fn exit_line(text: &str) -> Option<(usize, usize, i64)> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("command exited with code ")?;
    let start_num = at + "command exited with code ".len();
    let digits: String = text[start_num..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    let code = digits.parse().ok()?;
    Some((at, start_num + digits.len(), code))
}

/// App `parseToolOutput`: unwrap `{output}`, `{content}` (Cursor's `read`
/// answers `{content: "<the file>"}`), `{text}`, content block lists and
/// JSON strings, bounded in depth.
pub fn parse_output(raw: &Value) -> Output {
    parse_output_at(raw, 0)
}

fn parse_output_at(raw: &Value, depth: usize) -> Output {
    if depth >= OUTPUT_MAX_DEPTH {
        let (stdout, _) = cap(&raw.to_string());
        return Output {
            stdout,
            exit: None,
            truncated: true,
        };
    }
    match raw {
        Value::Null => Output::default(),
        Value::String(s) => {
            let trimmed = s.trim();
            if (trimmed.starts_with('[') && trimmed.ends_with(']'))
                || (trimmed.starts_with('{') && trimmed.ends_with('}'))
            {
                if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
                    return parse_output_at(&parsed, depth + 1);
                }
            }
            let (text, truncated) = cap(s);
            match exit_line(&text) {
                Some((start, end, code)) => {
                    let mut stdout = format!("{}{}", &text[..start], &text[end..]);
                    let trimmed_len = stdout.trim_end().trim_end_matches('.').trim_end().len();
                    stdout.truncate(trimmed_len);
                    Output {
                        stdout,
                        exit: Some(code),
                        truncated,
                    }
                }
                None => Output {
                    stdout: text,
                    exit: None,
                    truncated,
                },
            }
        }
        Value::Array(items) => {
            let mut out = Output::default();
            for item in items {
                let text = match item {
                    Value::String(s) => s.as_str(),
                    Value::Object(rec) => rec.get("text").and_then(Value::as_str).unwrap_or(""),
                    _ => "",
                };
                if text.is_empty() {
                    continue;
                }
                if let Some((_, _, code)) = exit_line(text).filter(|(s, _, _)| *s == 0) {
                    out.exit = Some(code);
                    continue;
                }
                if !out.stdout.is_empty() {
                    out.stdout.push('\n');
                }
                out.stdout.push_str(text);
                if out.stdout.len() > OUTPUT_BYTE_CAP {
                    out.stdout = cap(&out.stdout).0;
                    out.truncated = true;
                    break;
                }
            }
            out
        }
        Value::Object(rec) => {
            if let Some(stdout) = rec.get("stdout").and_then(Value::as_str) {
                // Cursor's `execute` answers `{stdout, stderr, exitCode}`.
                let stderr = rec.get("stderr").and_then(Value::as_str).unwrap_or("");
                let joined = match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
                    (_, true) => stdout.to_string(),
                    (true, false) => stderr.to_string(),
                    (false, false) => format!("{stdout}\n{stderr}"),
                };
                let (stdout, truncated) = cap(&joined);
                let exit = rec
                    .get("exitCode")
                    .or_else(|| rec.get("exit"))
                    .or_else(|| rec.get("exit_code"))
                    .and_then(Value::as_i64);
                return Output {
                    stdout,
                    exit,
                    truncated,
                };
            }
            if let Some(output) = rec.get("output") {
                let mut out = parse_output_at(output, depth + 1);
                // Antigravity's `{exit_code, output}`.
                out.exit = out.exit.or_else(|| {
                    rec.get("exit_code")
                        .or_else(|| rec.get("exitCode"))
                        .and_then(Value::as_i64)
                });
                return out;
            }
            if let Some(content @ (Value::Array(_) | Value::String(_))) = rec.get("content") {
                return parse_output_at(content, depth + 1);
            }
            if let Some(text) = rec.get("text") {
                return parse_output_at(text, depth + 1);
            }
            let pretty = serde_json::to_string_pretty(raw).unwrap_or_default();
            let (stdout, truncated) = cap(&pretty);
            Output {
                stdout,
                exit: None,
                truncated,
            }
        }
        other => Output {
            stdout: other.to_string(),
            exit: None,
            truncated: false,
        },
    }
}

/// The text a call returned: `content[]` text items first (the real
/// result), else `output` unwrapped.
pub fn output_text(call: &ToolCall) -> Output {
    let parsed = call.output.as_ref().map(parse_output).unwrap_or_default();
    if let Some(Value::Array(items)) = &call.content {
        // `{type: "text", text}`, or ACP's `{type: "content", content: {type: "text", text}}`.
        let text = items
            .iter()
            .map(|i| match i.get("content") {
                Some(inner) if i.get("type").and_then(Value::as_str) == Some("content") => inner,
                _ => i,
            })
            .filter(|i| i.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            let inner = parse_output(&Value::String(text));
            return Output {
                exit: inner.exit.or(parsed.exit),
                truncated: inner.truncated || parsed.truncated || call.truncated,
                stdout: inner.stdout,
            };
        }
    }
    Output {
        truncated: parsed.truncated || call.truncated,
        ..parsed
    }
}

fn metadata(call: &ToolCall) -> Option<&Map<String, Value>> {
    call.metadata.as_ref().and_then(Value::as_object)
}

/// One file of a change: its path, its unified patch and its counts.
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub patch: String,
    pub added: usize,
    pub removed: usize,
}

fn count_marked(patch: &str, marker: char) -> usize {
    let header: String = std::iter::repeat_n(marker, 3).collect();
    patch
        .lines()
        .filter(|l| l.starts_with(marker) && !l.starts_with(&header))
        .count()
}

impl FileDiff {
    fn new(path: String, patch: String, counts: Option<(usize, usize)>) -> Self {
        let (added, removed) =
            counts.unwrap_or_else(|| (count_marked(&patch, '+'), count_marked(&patch, '-')));
        Self {
            path,
            patch,
            added,
            removed,
        }
    }
}

/// App `diffFilesFromMetadata`: `metadata.files[]` first, else a flat
/// `{filepath, diff}` pair. The same reading for a tool card and the
/// permission that asked for the change.
pub fn diff_files(meta: Option<&Map<String, Value>>) -> Vec<FileDiff> {
    let Some(meta) = meta else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in meta
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(rec) = entry.as_object() else {
            continue;
        };
        let path = pick(rec, &["file", "path", "filePath", "file_path"]).unwrap_or_default();
        let patch = pick(rec, &["patch", "diff"]).unwrap_or_default();
        if path.is_empty() || patch.trim().is_empty() {
            continue;
        }
        let counts = match (
            rec.get("additions").and_then(Value::as_u64),
            rec.get("deletions").and_then(Value::as_u64),
        ) {
            (Some(a), Some(d)) => Some((a as usize, d as usize)),
            _ => None,
        };
        out.push(FileDiff::new(path.into(), patch.into(), counts));
    }
    if !out.is_empty() {
        return out;
    }
    if let Some(patch) = pick(meta, &["diff", "patch"]).filter(|p| !p.trim().is_empty()) {
        let path =
            pick(meta, &["filepath", "filePath", "file", "path", "file_path"]).unwrap_or_default();
        out.push(FileDiff::new(path.into(), patch.into(), None));
    }
    out
}

pub fn call_diff_files(call: &ToolCall) -> Vec<FileDiff> {
    diff_files(metadata(call))
}

/// An edit's before and after from its input (`old_string` / `new_string`).
pub fn edit_strings(rec: Option<&Map<String, Value>>) -> Option<(String, String)> {
    let rec = rec?;
    let old = rec
        .get("oldString")
        .or_else(|| rec.get("old_string"))
        .and_then(Value::as_str);
    let new = rec
        .get("newString")
        .or_else(|| rec.get("new_string"))
        .and_then(Value::as_str);
    (old.is_some() || new.is_some())
        .then(|| (old.unwrap_or("").to_string(), new.unwrap_or("").to_string()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchAction {
    Add,
    Update,
    Delete,
    Move,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatchSection {
    pub action: PatchAction,
    pub path: String,
    /// A unified-diff body the diff view can read.
    pub patch: String,
}

/// App `parsePatchSections`: `apply_patch`'s `*** Update File: x` format as
/// per-file patches.
pub fn parse_patch_sections(text: &str) -> Vec<PatchSection> {
    let mut sections = Vec::new();
    let mut current: Option<(PatchAction, String, Vec<&str>)> = None;
    let flush = |current: &mut Option<(PatchAction, String, Vec<&str>)>,
                 out: &mut Vec<PatchSection>| {
        if let Some((action, path, lines)) = current.take() {
            let body = lines.join("\n").trim_end().to_string();
            let (added, removed) = (
                lines.iter().filter(|l| l.starts_with('+')).count(),
                lines.iter().filter(|l| l.starts_with('-')).count(),
            );
            let patch = if body.is_empty() {
                String::new()
            } else {
                let body = lines
                    .iter()
                    .filter(|l| !l.starts_with("@@"))
                    .map(|l| {
                        if l.starts_with(['+', '-', ' ']) {
                            l.to_string()
                        } else {
                            format!(" {l}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let old = removed + body.lines().filter(|l| l.starts_with(' ')).count();
                let new = added + body.lines().filter(|l| l.starts_with(' ')).count();
                format!("--- a/{path}\n+++ b/{path}\n@@ -1,{old} +1,{new} @@\n{body}")
            };
            out.push(PatchSection {
                action,
                path,
                patch,
            });
        }
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("*** ") {
            let mut words = rest.splitn(2, " File:");
            let verb = words.next().unwrap_or("").trim().to_ascii_lowercase();
            if let Some(path) = words.next() {
                let action = match verb.as_str() {
                    "add" => Some(PatchAction::Add),
                    "update" => Some(PatchAction::Update),
                    "delete" => Some(PatchAction::Delete),
                    "move" => Some(PatchAction::Move),
                    _ => None,
                };
                if let Some(action) = action {
                    flush(&mut current, &mut sections);
                    current = Some((action, path.trim().to_string(), Vec::new()));
                    continue;
                }
            }
            if rest
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("end patch")
            {
                flush(&mut current, &mut sections);
            }
            continue;
        }
        if let Some((_, _, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    flush(&mut current, &mut sections);
    sections
}

/// One folder of a `glob` answer and the file names in it.
#[derive(Debug, Clone, PartialEq)]
pub struct PathGroup {
    pub directory: String,
    pub files: Vec<String>,
}

/// App `groupPathsByDirectory`, in first-seen order.
pub fn group_paths(output: &str) -> Vec<PathGroup> {
    let mut groups: Vec<PathGroup> = Vec::new();
    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() || line.contains(' ') {
            continue;
        }
        let (directory, name) = (dirname(line), basename(line));
        if name.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|g| g.directory == directory) {
            Some(group) => group.files.push(name.to_string()),
            None => groups.push(PathGroup {
                directory: directory.to_string(),
                files: vec![name.to_string()],
            }),
        }
    }
    groups
}

#[derive(Debug, Clone, PartialEq)]
pub struct GrepFile {
    pub file: String,
    pub lines: Vec<(Option<u64>, String)>,
}

/// App `groupGrepMatches`: OpenCode's `file:` / `  Line N: text` and the
/// `path:N:text` form ripgrep prints.
pub fn group_grep(output: &str) -> Vec<GrepFile> {
    let mut groups: Vec<GrepFile> = Vec::new();
    for raw in output.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        if lower.starts_with("found ") && lower.contains(" match") {
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            let t = raw.trim_start();
            if let Some(rest) = t.strip_prefix("Line ") {
                if let Some((n, text)) = rest.split_once(':') {
                    if let (Ok(n), Some(g)) = (n.trim().parse(), groups.last_mut()) {
                        g.lines
                            .push((Some(n), text.strip_prefix(' ').unwrap_or(text).into()));
                        continue;
                    }
                }
            }
        } else {
            let mut parts = raw.splitn(3, ':');
            if let (Some(file), Some(n), Some(text)) = (parts.next(), parts.next(), parts.next()) {
                if let Ok(n) = n.parse::<u64>() {
                    if groups.last().is_none_or(|g| g.file != file) {
                        groups.push(GrepFile {
                            file: file.into(),
                            lines: Vec::new(),
                        });
                    }
                    if let Some(g) = groups.last_mut() {
                        g.lines.push((Some(n), text.into()));
                    }
                    continue;
                }
            }
            if let Some(file) = raw.strip_suffix(':') {
                groups.push(GrepFile {
                    file: file.into(),
                    lines: Vec::new(),
                });
                continue;
            }
        }
        match groups.last_mut() {
            Some(g) => g.lines.push((None, raw.trim().to_string())),
            None => groups.push(GrepFile {
                file: raw.trim().to_string(),
                lines: Vec::new(),
            }),
        }
    }
    groups
}

/// A line's `N:` / `N→` / `N<tab>` numbering prefix: (number, the rest).
fn numbered(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &t[digits..];
    rest.strip_prefix('→')
        .or_else(|| rest.strip_prefix('\t'))
        .or_else(|| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(':')))
}

/// App `stripReadLineNumbers`, also for Claude's `     1→text` and `cat -n`.
/// Only a listing where every non-empty line is numbered is stripped (or
/// one under OpenCode's `Read file` header); a file whose own text has
/// "12: x" on one line is left alone.
pub fn strip_read_line_numbers(output: &str) -> String {
    let lines: Vec<&str> = output.split('\n').collect();
    let header = lines.first().is_some_and(|l| l.starts_with("Read file "));
    let body = if header { &lines[1..] } else { &lines[..] };
    // OpenCode wraps the listing in `<file>` … `</file>` with a footer.
    let body: Vec<&str> = body
        .iter()
        .copied()
        .filter(|l| !matches!(l.trim(), "<file>" | "</file>"))
        .filter(|l| !(l.starts_with("(End of file") || l.starts_with("(File has more lines")))
        .collect();
    if !header {
        let meaningful: Vec<&&str> = body.iter().filter(|l| !l.trim().is_empty()).collect();
        if meaningful.is_empty() || !meaningful.iter().all(|l| numbered(l).is_some()) {
            return output.to_string();
        }
    }
    body.iter()
        .map(|l| numbered(l).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A URL as host (no `www.`) and path.
pub fn split_url(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.trim().split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    {
        return None;
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..end].trim_start_matches("www.").to_string();
    let path = rest[end..].split('#').next().unwrap_or("");
    let path = if path == "/" { "" } else { path };
    (!host.is_empty()).then(|| (host, path.to_string()))
}

pub fn shell_exit(call: &ToolCall, parsed: &Output) -> Option<i64> {
    metadata(call)
        .and_then(|m| m.get("exit").or_else(|| m.get("exit_code")))
        .and_then(Value::as_i64)
        .or(parsed.exit)
}

pub fn shell_timed_out(call: &ToolCall) -> bool {
    metadata(call).is_some_and(|m| {
        m.get("timeout") == Some(&Value::Bool(true))
            || m.get("status").and_then(Value::as_str) == Some("timeout")
    })
}

pub fn result_count(call: &ToolCall) -> Option<u64> {
    let keys = ["count", "matches", "results", "total", "totalMatches"];
    let find = |m: &Map<String, Value>| keys.iter().find_map(|k| m.get(*k).and_then(Value::as_u64));
    // Cursor's search answers `{totalMatches, truncated}` with no text.
    metadata(call).and_then(find).or_else(|| {
        call.output
            .as_ref()
            .and_then(Value::as_object)
            .and_then(find)
    })
}

/// A shell command as a person reads it: Codex runs every command as
/// `/usr/bin/bash -lc '<command>'`, and the wrapper is not the command.
pub fn display_command(command: &str) -> String {
    let t = command.trim();
    for shell in ["bash", "zsh", "sh"] {
        for flag in ["-lc", "-c"] {
            let lead = [
                format!("/usr/bin/{shell} {flag} "),
                format!("/bin/{shell} {flag} "),
                format!("{shell} {flag} "),
            ];
            if let Some(rest) = lead.iter().find_map(|l| t.strip_prefix(l.as_str())) {
                let rest = rest.trim();
                for q in ['\'', '"'] {
                    if rest.len() >= 2 && rest.starts_with(q) && rest.ends_with(q) {
                        return rest[1..rest.len() - 1].to_string();
                    }
                }
                return rest.to_string();
            }
        }
    }
    t.to_string()
}

/// One web search hit (Codex's `web_search` answers a list of these).
#[derive(Debug, Clone, PartialEq)]
pub struct WebHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub fn web_hits(call: &ToolCall) -> Vec<WebHit> {
    let list = call.output.as_ref().and_then(Value::as_array).or_else(|| {
        metadata(call)
            .and_then(|m| m.get("results"))
            .and_then(Value::as_array)
    });
    list.into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|r| {
            let url = pick(r, &["url", "link"])?.to_string();
            Some(WebHit {
                title: pick(r, &["title"]).unwrap_or(&url).to_string(),
                snippet: pick(r, &["snippet", "description"])
                    .unwrap_or_default()
                    .to_string(),
                url,
            })
        })
        .collect()
}

pub fn meta_str<'a>(call: &'a ToolCall, keys: &[&str]) -> Option<&'a str> {
    pick(metadata(call)?, keys)
}

/// One call Code Mode made.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecuteCall {
    pub name: String,
    /// `None` running, `Some(false)` failed, `Some(true)` completed.
    pub ok: Option<bool>,
    pub input: String,
}

pub fn execute_calls(call: &ToolCall) -> Vec<ExecuteCall> {
    let Some(m) = metadata(call) else {
        return Vec::new();
    };
    let raw = m.get("toolCalls").or_else(|| m.get("tool_calls"));
    let mut out = Vec::new();
    for entry in raw.and_then(Value::as_array).into_iter().flatten() {
        if let Some(name) = entry.as_str().filter(|s| !s.is_empty()) {
            out.push(ExecuteCall {
                name: name.into(),
                ok: Some(true),
                input: String::new(),
            });
            continue;
        }
        let Some(rec) = entry.as_object() else {
            continue;
        };
        let Some(name) = pick(rec, &["tool", "name", "id"]) else {
            continue;
        };
        let status = rec
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let ok = match status.as_str() {
            "error" | "failed" | "rejected" => Some(false),
            "running" | "pending" | "started" => None,
            _ => Some(true),
        };
        let input = match rec.get("input") {
            Some(Value::String(s)) => first_line(s).chars().take(120).collect(),
            Some(Value::Object(o)) => argument_line(o).chars().take(120).collect(),
            _ => String::new(),
        };
        out.push(ExecuteCall {
            name: name.into(),
            ok,
            input,
        });
    }
    out
}

pub fn execute_errored(call: &ToolCall) -> bool {
    metadata(call).is_some_and(|m| m.get("error") == Some(&Value::Bool(true)))
}

/// App `toolArgumentLine`: `key value` pairs of a streaming input, one line.
pub fn argument_line(rec: &Map<String, Value>) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in rec {
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        if text.is_empty() {
            continue;
        }
        parts.push(format!("{key} {text}"));
        if parts.join("  ").len() >= 240 {
            break;
        }
    }
    parts.join("  ").chars().take(240).collect()
}

/// A subagent's result with OpenCode's `<subagent …>` envelope taken off.
pub fn strip_subagent_envelope(raw: &str) -> (String, Option<String>) {
    let lower = raw.to_ascii_lowercase();
    let Some(open) = lower.find("<subagent") else {
        return (raw.to_string(), None);
    };
    let Some(close_tag) = raw[open..].find('>') else {
        return (raw.to_string(), None);
    };
    let attrs = &raw[open..open + close_tag];
    let state = attrs.find("state=\"").and_then(|at| {
        let rest = &attrs[at + 7..];
        rest.find('"').map(|end| rest[..end].to_string())
    });
    let body_start = open + close_tag + 1;
    let body = match lower.rfind("</subagent>") {
        Some(close) if close > body_start => &raw[body_start..close],
        _ => &raw[body_start..],
    };
    (body.trim().to_string(), state)
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    pub multiple: bool,
}

/// App `parseToolQuestions`: `{questions: [{question, header, options, multiple}]}`
/// (Claude's `AskUserQuestion` says `multiSelect`).
pub fn parse_questions(rec: &Map<String, Value>) -> Vec<Question> {
    let mut out = Vec::new();
    for entry in rec
        .get("questions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(q) = entry.as_object() else {
            continue;
        };
        let question = pick(q, &["question", "prompt", "text"]).unwrap_or_default();
        let header = pick(q, &["header", "title", "label"]).unwrap_or_default();
        if question.is_empty() && header.is_empty() {
            continue;
        }
        let mut options = Vec::new();
        for raw in q
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match raw {
                Value::String(s) if !s.is_empty() => options.push(QuestionOption {
                    label: s.clone(),
                    description: None,
                }),
                Value::Object(o) => {
                    if let Some(label) = pick(o, &["label", "value", "title"]) {
                        options.push(QuestionOption {
                            label: label.into(),
                            description: pick(o, &["description", "detail", "hint"])
                                .map(str::to_string),
                        });
                    }
                }
                _ => {}
            }
        }
        let multiple = [q.get("multiple"), q.get("multiSelect")]
            .into_iter()
            .flatten()
            .any(|v| v == &Value::Bool(true));
        out.push(Question {
            header: header.into(),
            question: question.into(),
            options,
            multiple,
        });
    }
    out
}

/// App `questionAnswersFromMetadata`: one list per question.
pub fn question_answers(call: &ToolCall) -> Vec<Vec<String>> {
    let Some(raw) = metadata(call)
        .and_then(|m| m.get("answers"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    raw.iter()
        .map(|entry| match entry {
            Value::String(s) if !s.is_empty() => vec![s.clone()],
            Value::Array(list) => list
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// A todo tool's checklist (`todos` / `items`, `content` / `text`).
pub fn todo_items(rec: Option<&Map<String, Value>>) -> Vec<(String, bool)> {
    let Some(rec) = rec else {
        return Vec::new();
    };
    let raw = rec.get("todos").or_else(|| rec.get("items"));
    let mut out = Vec::new();
    for entry in raw.and_then(Value::as_array).into_iter().flatten() {
        match entry {
            Value::String(s) => out.push((s.clone(), false)),
            Value::Object(o) => {
                if let Some(text) = pick(o, &["content", "text"]) {
                    // `completed`, Cursor's `TODO_STATUS_COMPLETED`, `done`.
                    let status = o
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    let done = status.ends_with("completed")
                        || status.ends_with("done")
                        || o.get("done") == Some(&Value::Bool(true));
                    out.push((text.into(), done));
                }
            }
            _ => {}
        }
    }
    out
}

/// A `sleep` call's duration in ms.
pub fn sleep_ms(call: &ToolCall, rec: Option<&Map<String, Value>>) -> Option<u64> {
    if call.name != "sleep" {
        return None;
    }
    let rec = rec?;
    rec.get("durationMs")
        .or_else(|| rec.get("duration_ms"))
        .and_then(Value::as_f64)
        .filter(|d| d.is_finite() && *d >= 0.0)
        .map(|d| d as u64)
}

/// `time.completed - time.ran` (or `- created`), when both are known.
pub fn duration_ms(call: &ToolCall) -> Option<u64> {
    let start = call.time.ran.or(call.time.created)?;
    call.time.completed?.checked_sub(start)
}

pub fn pending(status: ToolCallStatus) -> bool {
    matches!(
        status,
        ToolCallStatus::Pending | ToolCallStatus::Streaming | ToolCallStatus::Running
    )
}

/// App `isDeclinedByUser`: the engine saying the reader said no.
pub fn declined_by_user(message: &str) -> bool {
    let t = message.trim().to_ascii_lowercase();
    let verbs = ["declined", "denied", "rejected"];
    let lead = t
        .strip_prefix("the ")
        .unwrap_or(&t)
        .strip_prefix("user ")
        .is_some_and(|rest| verbs.iter().any(|v| rest.starts_with(v)));
    lead || verbs
        .iter()
        .any(|v| t.contains(&format!("{v} by user")) || t.contains(&format!("{v} by the user")))
}

// ---- Muqun's own MCP tools ---------------------------------------------------

/// The Muqun MCP operations (App `internal-muqun-tools.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuqunOp {
    ExecRun,
    FilesPresent,
    TasksDelegate,
    TasksWait,
    TasksRead,
    TasksCancel,
    TasksComplete,
    FleetList,
    FleetDescribe,
}

const MUQUN_OPS: &[(&str, MuqunOp)] = &[
    ("exec.run", MuqunOp::ExecRun),
    ("files.present", MuqunOp::FilesPresent),
    ("tasks.delegate", MuqunOp::TasksDelegate),
    ("tasks.wait", MuqunOp::TasksWait),
    ("tasks.read", MuqunOp::TasksRead),
    ("tasks.cancel", MuqunOp::TasksCancel),
    ("tasks.complete", MuqunOp::TasksComplete),
    ("fleet.list", MuqunOp::FleetList),
    ("fleet.describe", MuqunOp::FleetDescribe),
];

pub fn muqun_op(name: &str) -> Option<MuqunOp> {
    let tool = ["mcp__muqun__", "muqun__", "muqun_", "muqun.", "muqun:"]
        .iter()
        .find_map(|p| name.strip_prefix(p))?;
    MUQUN_OPS
        .iter()
        .find(|(op, _)| tool == *op || tool == op.replace('.', "_"))
        .map(|(_, op)| *op)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuqunStatus {
    Calling,
    Started,
    Working,
    Queued,
    Waiting,
    Unconfirmed,
    Done,
    Failed,
    Cancelled,
    Returned,
    Unassigned,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PresentedFile {
    pub path: String,
    pub name: String,
    pub mime: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MuqunMachine {
    pub label: String,
    pub os: Option<String>,
    pub online: Option<bool>,
    pub unauthorized: bool,
}

/// App `MuqunToolPresentation`.
#[derive(Debug, Clone, PartialEq)]
pub struct Muqun {
    pub op: MuqunOp,
    pub status: MuqunStatus,
    pub files: Vec<PresentedFile>,
    pub machine: Option<String>,
    pub task_id: Option<String>,
    pub cwd: Option<String>,
    pub command: Option<String>,
    pub goal: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    pub exit: Option<i64>,
    pub note: Option<String>,
    pub timeline_count: Option<usize>,
    pub agents: Option<String>,
    pub toolchains: Option<String>,
    pub gateway_version: Option<String>,
    pub machines: Vec<MuqunMachine>,
}

fn text_of(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(|s| cap(s).0)
}

/// App `resultRecord`: bounded unwrapping of an MCP result to its record.
fn result_record(value: &Value, depth: usize) -> Option<Map<String, Value>> {
    if depth > 5 {
        return None;
    }
    match value {
        Value::String(s) if s.len() <= OUTPUT_BYTE_CAP => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| result_record(&v, depth + 1)),
        Value::Array(_) => {
            let mut wrap = Map::new();
            wrap.insert("content".into(), value.clone());
            result_record(&Value::Object(wrap), depth + 1)
        }
        Value::Object(rec) => {
            if rec.get("isError") == Some(&Value::Bool(true)) {
                let message = rec
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .take(8)
                    .filter_map(|b| text_of(b.get("text")))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut flat = rec.clone();
                flat.insert("isError".into(), Value::Bool(false));
                let mut nested = result_record(&Value::Object(flat), depth + 1).unwrap_or_default();
                if !nested.contains_key("error") {
                    let err = text_of(nested.get("message"))
                        .or((!message.is_empty()).then_some(message))
                        .map_or(Value::Bool(true), Value::String);
                    nested.insert("error".into(), err);
                }
                return Some(nested);
            }
            if let Some(own) = rec.get("your_task") {
                return result_record(own, depth + 1).or_else(|| Some(rec.clone()));
            }
            if ["task_id", "state", "machines", "machineId", "error"]
                .iter()
                .any(|k| rec.contains_key(*k))
            {
                return Some(rec.clone());
            }
            if let Some(s) = rec.get("structuredContent").filter(|v| !v.is_null()) {
                return result_record(s, depth + 1);
            }
            if let Some(r) = rec.get("result").filter(|v| !v.is_null()) {
                return result_record(r, depth + 1);
            }
            for block in rec
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(8)
            {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(found) = block.get("text").and_then(|t| result_record(t, depth + 1))
                    {
                        return Some(found);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn presented_files(result: &Map<String, Value>) -> Vec<PresentedFile> {
    let server_ok = result
        .get("server_id")
        .and_then(Value::as_str)
        .is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        });
    if !server_ok {
        return Vec::new();
    }
    let mut out = Vec::new();
    for file in result
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(8)
    {
        let Some(rec) = file.as_object() else {
            continue;
        };
        let Some(path) = rec.get("path").and_then(Value::as_str) else {
            continue;
        };
        let rooted = path.starts_with('/') || path.starts_with("~/");
        if !rooted
            || path.starts_with("//")
            || path.len() > 8192
            || path.chars().any(|c| c.is_control())
        {
            continue;
        }
        let name = pick(rec, &["name"])
            .map(str::to_string)
            .unwrap_or_else(|| basename(path).to_string());
        if name.len() > 256 || name.chars().any(|c| c.is_control()) {
            continue;
        }
        out.push(PresentedFile {
            path: path.into(),
            name,
            mime: pick(rec, &["mime"]).map(str::to_string),
            size: rec.get("size").and_then(Value::as_u64),
        });
    }
    out
}

fn named_versions(value: Option<&Value>) -> Option<String> {
    let labels: Vec<String> = value?
        .as_array()?
        .iter()
        .take(16)
        .filter_map(|row| {
            let id = text_of(row.get("id"))?;
            Some(match text_of(row.get("version")) {
                Some(v) => format!("{id} {v}"),
                None => id,
            })
        })
        .collect();
    (!labels.is_empty()).then(|| labels.join(" · "))
}

/// App `muqunToolPresentation`.
pub fn muqun(call: &ToolCall) -> Option<Muqun> {
    let op = muqun_op(&call.name)?;
    let input = input_record(call).unwrap_or_default();
    let result = call
        .output
        .as_ref()
        .and_then(|o| result_record(o, 0))
        .or_else(|| {
            call.content.as_ref().and_then(|c| {
                let mut wrap = Map::new();
                wrap.insert("content".into(), c.clone());
                result_record(&Value::Object(wrap), 0)
            })
        });
    let r = result.as_ref();
    let get = |k: &str| r.and_then(|r| r.get(k));
    let state = get("state").and_then(Value::as_str);
    let busy = pending(call.state);
    let exit = get("exit_code").and_then(Value::as_i64);
    let error_rec = get("error").and_then(Value::as_object);
    let error_text = call
        .error
        .as_ref()
        .map(|e| e.message.clone())
        .filter(|m| !m.trim().is_empty())
        .or_else(|| text_of(error_rec.and_then(|e| e.get("message"))))
        .or_else(|| text_of(get("message")));
    let has_error = get("error").is_some_and(|e| !e.is_null() && e != &Value::Bool(false));
    let failed = call.state == ToolCallStatus::Failed
        || has_error
        || state == Some("failed")
        || (state != Some("cancelled") && exit.is_some_and(|e| e != 0));
    let status = if failed {
        MuqunStatus::Failed
    } else if busy {
        MuqunStatus::Calling
    } else {
        match state {
            Some("done") => MuqunStatus::Done,
            Some("cancelled") => MuqunStatus::Cancelled,
            Some("waiting") => MuqunStatus::Waiting,
            Some("idle_unconfirmed") => MuqunStatus::Unconfirmed,
            Some("queued" | "dispatching") => MuqunStatus::Queued,
            Some("working") if matches!(op, MuqunOp::ExecRun | MuqunOp::TasksDelegate) => {
                MuqunStatus::Started
            }
            Some("working") => MuqunStatus::Working,
            _ if r.is_some_and(|r| r.get("your_task") == Some(&Value::Null)) => {
                MuqunStatus::Unassigned
            }
            _ if r.is_some()
                && (matches!(op, MuqunOp::FleetList | MuqunOp::FleetDescribe)
                    || get("items").is_some_and(Value::is_array)) =>
            {
                MuqunStatus::Returned
            }
            _ => MuqunStatus::Unknown,
        }
    };
    let command = if op == MuqunOp::ExecRun {
        text_of(input.get("command"))
    } else if get("kind").and_then(Value::as_str) == Some("command") {
        text_of(get("goal"))
    } else {
        None
    };
    let goal = if command.is_some() {
        None
    } else {
        text_of(get("goal")).or_else(|| text_of(input.get("goal")))
    };
    let items = get("items").and_then(Value::as_array);
    let timeline_output = items.map(|items| {
        items
            .iter()
            .skip(items.len().saturating_sub(8))
            .filter_map(|i| text_of(i.get("text")))
            .collect::<Vec<_>>()
            .join("\n")
    });
    let output = ["stdout_tail", "answer", "summary", "progress"]
        .iter()
        .find_map(|k| text_of(get(k)))
        .or_else(|| timeline_output.filter(|t| !t.trim().is_empty()))
        .filter(|o| Some(o) != command.as_ref() && Some(o) != goal.as_ref());
    let mut machines = Vec::new();
    let source: Vec<&Map<String, Value>> = match get("machines").and_then(Value::as_array) {
        Some(list) => list.iter().take(50).filter_map(Value::as_object).collect(),
        None if get("machineId").is_some() => r.into_iter().collect(),
        None => Vec::new(),
    };
    for m in source {
        if let (Some(_), Some(label)) = (text_of(m.get("machineId")), text_of(m.get("label"))) {
            machines.push(MuqunMachine {
                label,
                os: text_of(m.get("os")),
                online: m.get("online").and_then(Value::as_bool),
                unauthorized: m.get("state").and_then(Value::as_str) == Some("unauthorized"),
            });
        }
    }
    let binding = get("binding").and_then(Value::as_object);
    Some(Muqun {
        op,
        status,
        files: match r {
            Some(r) if !busy && !failed => presented_files(r),
            _ => Vec::new(),
        },
        machine: text_of(get("machine"))
            .or_else(|| text_of(binding.and_then(|b| b.get("peer_name"))))
            .or_else(|| text_of(input.get("machine"))),
        task_id: text_of(get("task_id")).or_else(|| text_of(input.get("task_id"))),
        cwd: text_of(input.get("cwd")),
        command,
        goal,
        output,
        error: if failed {
            error_text.or_else(|| text_of(get("error")))
        } else {
            None
        },
        exit,
        note: text_of(get("note")),
        timeline_count: items.map(Vec::len),
        agents: named_versions(get("agents")),
        toolchains: named_versions(get("toolchains")),
        gateway_version: text_of(get("gatewayVersion")),
        machines,
    })
}

// ---- Permissions -------------------------------------------------------------

/// App `permissionSubject`: the one thing a permission is about, with the
/// rule key taken off its front.
pub fn permission_subject(request: &PermissionRequest) -> String {
    let prompt = request.prompt.trim();
    let subject = if prompt.is_empty() {
        request.resources.first().map(String::as_str).unwrap_or("")
    } else {
        prompt
    };
    let action = request.action.trim();
    if action.is_empty() || subject.is_empty() {
        return subject.to_string();
    }
    let prefix = format!("{}:", action.to_ascii_lowercase());
    if subject.to_ascii_lowercase().starts_with(&prefix) {
        let rest = subject[prefix.len()..].trim();
        if !rest.is_empty() {
            return rest.to_string();
        }
        return request
            .resources
            .first()
            .cloned()
            .unwrap_or_else(|| subject.to_string());
    }
    subject.to_string()
}

const CRITICAL_ROOT_DIRS: &[&str] = &[
    "/bin", "/boot", "/dev", "/etc", "/lib", "/lib64", "/proc", "/root", "/sbin", "/sys", "/usr",
    "/var",
];

/// Why a shell command is irreversibly destructive (App
/// `dangerousShellReason`), for the warning on its approval.
pub fn dangerous_shell_reason(raw: &str) -> Option<&'static str> {
    let lower = raw.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split_whitespace()
        .filter(|w| !matches!(*w, "sudo" | "doas" | "env"))
        .collect();
    // rm with both r and f flags aimed at a root or a system directory.
    for (i, w) in words.iter().enumerate() {
        if *w != "rm" && !w.ends_with("/rm") {
            continue;
        }
        let mut flags = String::new();
        let mut j = i + 1;
        while let Some(f) = words.get(j).filter(|f| f.starts_with('-')) {
            if f.trim_start_matches('-') == "no-preserve-root" {
                return Some("rm-system-root");
            }
            flags.push_str(f);
            j += 1;
        }
        let recursive = flags.contains('r') || flags.contains("recursive");
        let force = flags.contains('f') || flags.contains("force");
        if let Some(target) = words.get(j).filter(|_| recursive && force) {
            let root = target.trim_end_matches('/');
            if root.is_empty()
                || target.starts_with("/*")
                || *target == "~"
                || target.starts_with("~/") && target.len() <= 2
                || *target == "$home"
                || *target == "${home}"
                || CRITICAL_ROOT_DIRS.contains(&root)
            {
                return Some("rm-system-root");
            }
        }
    }
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains(":(){:|:&};:") || compact.contains(":(){:|:&}") {
        return Some("fork-bomb");
    }
    let device = ["/dev/sd", "/dev/hd", "/dev/vd", "/dev/nvme", "/dev/mmcblk"];
    let writes_device = device.iter().any(|d| {
        lower.contains(&format!(">{d}"))
            || lower.contains(&format!("> {d}"))
            || lower.contains(&format!("of={d}"))
    });
    if writes_device {
        return Some("block-device-write");
    }
    if words.iter().any(|w| w.starts_with("mkfs")) && device.iter().any(|d| lower.contains(d)) {
        return Some("disk-format");
    }
    if words.first().is_some_and(|w| *w == "chmod")
        && words.last().is_some_and(|w| *w == "/" || *w == "/*")
        && words.iter().any(|w| {
            matches!(
                *w,
                "777" | "000" | "a+rwx" | "a+wx" | "o+w" | "go+w" | "ugo+w"
            )
        })
    {
        return Some("chmod-system-root");
    }
    let pipes_remote = (lower.contains("curl ") || lower.contains("wget "))
        && lower.split('|').skip(1).any(|seg| {
            let seg = seg.trim_start().trim_start_matches("sudo ").trim_start();
            seg.starts_with("sh") || seg.starts_with("bash")
        });
    if pipes_remote || lower.contains("sh <(curl") || lower.contains("sh <(wget") {
        return Some("remote-pipe-shell");
    }
    None
}

/// A shell permission whose command would do whole-system damage.
pub fn dangerous_permission(request: &PermissionRequest) -> Option<&'static str> {
    if !matches!(request.action.as_str(), "shell" | "bash" | "command") {
        return None;
    }
    request
        .resources
        .iter()
        .chain(std::iter::once(&request.prompt))
        .find_map(|r| dangerous_shell_reason(r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, input: Value, output: Value) -> ToolCall {
        serde_json::from_value(json!({
            "id": "c", "name": name, "input": input, "output": output,
            "state": "completed", "status": "completed",
        }))
        .unwrap()
    }

    #[test]
    fn read_listings_lose_their_numbers_but_files_keep_theirs() {
        let claude = "     1→fn main() {\n     2→}\n";
        assert_eq!(strip_read_line_numbers(claude), "fn main() {\n}\n");
        let opencode = "Read file x\n<file>\n00001| a\n</file>";
        // OpenCode's `N| ` form is not a numbering we recognise: left as is.
        assert!(strip_read_line_numbers(opencode).contains("00001| a"));
        let header = "Read file src/a.rs\n1: one\n2: two";
        assert_eq!(strip_read_line_numbers(header), "one\ntwo");
        let prose = "intro\n12: not a listing";
        assert_eq!(strip_read_line_numbers(prose), prose);
    }

    #[test]
    fn outputs_unwrap_like_the_app() {
        // Cursor's read answers `{content: "<file>"}`.
        let c = call(
            "read",
            json!({"path": "/a/b.rs"}),
            json!({"content": "x = 1"}),
        );
        assert_eq!(output_text(&c).stdout, "x = 1");
        let shell = parse_output(&json!([{"text": "ok"}, {"text": "Command exited with code 2."}]));
        assert_eq!((shell.stdout.as_str(), shell.exit), ("ok", Some(2)));
        let nested = parse_output(&json!({"output": "{\"stdout\": \"hi\", \"exitCode\": 0}"}));
        assert_eq!((nested.stdout.as_str(), nested.exit), ("hi", Some(0)));
    }

    #[test]
    fn real_engine_shapes_read_right() {
        // Claude over ACP wraps text in `{type: "content", content: {…}}`.
        let mut c = call("Bash", json!({"command": "ls"}), Value::Null);
        c.content = Some(json!([{"type": "content", "content": {"type": "text", "text": "a.rs"}}]));
        assert_eq!(output_text(&c).stdout, "a.rs");
        // Cursor's execute, Antigravity's run_command.
        let o = parse_output(&json!({"exitCode": 1, "stdout": "", "stderr": "boom"}));
        assert_eq!((o.stdout.as_str(), o.exit), ("boom", Some(1)));
        let o = parse_output(&json!({"exit_code": 2, "output": "x"}));
        assert_eq!(o.exit, Some(2));
        assert_eq!(
            display_command("/usr/bin/bash -lc 'git status --short'"),
            "git status --short"
        );
        assert_eq!(classify("updateTodos"), ToolKind::Todo);
        assert_eq!(classify("run_command"), ToolKind::Shell);
        let codex_edit = call("edit", json!({"paths": ["src/a.rs"]}), Value::Null);
        let rec = input_record(&codex_edit);
        assert_eq!(target(ToolKind::Edit, rec.as_ref()), "src/a.rs");
    }

    #[test]
    fn streaming_input_yields_the_values_so_far() {
        let mut c = call("bash", Value::Null, Value::Null);
        c.input_partial = Some(r#"{"description": "x", "command": "git sta"#.into());
        let rec = input_record(&c);
        assert_eq!(target(ToolKind::Shell, rec.as_ref()), "git sta");
    }

    #[test]
    fn grep_and_glob_group_by_file_and_folder() {
        let grep =
            group_grep("Found 3 matches\n/a/x.rs:\n  Line 3: foo\n  Line 9: bar\nsrc/y.rs:4:baz");
        assert_eq!(grep.len(), 2);
        assert_eq!(
            grep[0].lines,
            vec![(Some(3), "foo".into()), (Some(9), "bar".into())]
        );
        assert_eq!(grep[1].file, "src/y.rs");
        let glob = group_paths("/r/src/a.rs\n/r/src/b.rs\n/r/c.rs\nNo more results here");
        assert_eq!(glob.len(), 2);
        assert_eq!(glob[0].files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn apply_patch_sections_become_unified_diffs() {
        let text = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n ctx\n*** Add File: b.txt\n+hi\n*** End Patch";
        let sections = parse_patch_sections(text);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].action, PatchAction::Update);
        assert!(sections[0]
            .patch
            .contains("@@ -1,2 +1,2 @@\n-old\n+new\n ctx"));
        assert_eq!(sections[1].path, "b.txt");
    }

    #[test]
    fn muqun_results_read_like_the_app() {
        let mut c = call(
            "mcp__muqun__files_present",
            json!({"paths": ["/tmp/r.pdf"]}),
            json!({"content": [{"type": "text", "text": "{\"server_id\":\"s1\",\"state\":\"done\",\"files\":[{\"path\":\"/tmp/r.pdf\",\"name\":\"r.pdf\",\"size\":10}]}"}]}),
        );
        let m = muqun(&c).unwrap();
        assert_eq!((m.op, m.status), (MuqunOp::FilesPresent, MuqunStatus::Done));
        assert_eq!(m.files[0].name, "r.pdf");
        c.name = "mcp__muqun__exec_run".into();
        c.input = json!({"command": "ls", "machine": "mini"});
        c.output = Some(json!({"state": "done", "exit_code": 3, "stdout_tail": "boom"}));
        let m = muqun(&c).unwrap();
        assert_eq!(m.status, MuqunStatus::Failed);
        assert_eq!(m.command.as_deref(), Some("ls"));
        assert_eq!(m.machine.as_deref(), Some("mini"));
        assert!(muqun_op("muqun_tasks_wait").is_some() && muqun_op("other__tasks_wait").is_none());
    }

    #[test]
    fn destructive_commands_are_flagged_and_ordinary_ones_are_not() {
        for bad in [
            "sudo rm -rf /",
            "rm -fr /usr",
            "curl -sL x.sh | sh",
            "dd if=a of=/dev/sda",
        ] {
            assert!(dangerous_shell_reason(bad).is_some(), "{bad}");
        }
        for ok in [
            "rm -rf target",
            "cargo test",
            "curl -o x https://a",
            "chmod 755 ./x",
        ] {
            assert!(dangerous_shell_reason(ok).is_none(), "{ok}");
        }
    }
}
