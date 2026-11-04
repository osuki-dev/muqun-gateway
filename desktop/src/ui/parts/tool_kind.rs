//! Tool classification and header targets, matching the App's
//! `agent-tool-output.ts` / `agent-tool-card.tsx` so desktop cards name and
//! summarise the same families.

use serde_json::Value;

use crate::ui::glyphs::Glyph;

/// The tool families OpenCode ships, plus a bucket for MCP additions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Image,
    Mcp,
}

impl ToolKind {
    pub fn glyph(self) -> Glyph {
        match self {
            Self::Shell => Glyph::Terminal,
            Self::Read => Glyph::Document,
            Self::Edit => Glyph::EditFile,
            Self::Write => Glyph::AddFile,
            Self::Patch => Glyph::FileChanges,
            Self::Glob | Self::Search => Glyph::Search,
            Self::Grep => Glyph::RegularExpression,
            Self::Web => Glyph::Network,
            Self::Subagent => Glyph::Fork,
            Self::Skill => Glyph::Suggested,
            Self::Todo => Glyph::TaskList,
            Self::Question => Glyph::Help,
            Self::Execute => Glyph::Json,
            Self::Browser => Glyph::Devices,
            Self::Image => Glyph::Photo,
            Self::Mcp => Glyph::Tool,
        }
    }

    /// Completed calls of these kinds can sit as quiet one-liners (App routine).
    pub fn is_routine(self) -> bool {
        matches!(
            self,
            Self::Shell | Self::Read | Self::Glob | Self::Grep | Self::Search | Self::Web
        )
    }
}

pub fn classify(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "shell" | "bash" | "terminal" | "run" => ToolKind::Shell,
        "read" | "list" | "ls" | "cat" | "view" | "readfile" | "read_file" => ToolKind::Read,
        "edit" | "multiedit" | "str_replace" | "str_replace_editor" => ToolKind::Edit,
        "write" | "create" | "new_file" => ToolKind::Write,
        "patch" | "apply_patch" | "applypatch" => ToolKind::Patch,
        "glob" | "find" => ToolKind::Glob,
        "grep" | "ripgrep" => ToolKind::Grep,
        "search" | "codesearch" => ToolKind::Search,
        "webfetch" | "fetch" | "websearch" | "web_search" | "web_fetch" | "get" => ToolKind::Web,
        "subagent" | "task" | "spawn" | "agent" => ToolKind::Subagent,
        "skill" | "load_skill" => ToolKind::Skill,
        "todowrite" | "todoread" | "todo" | "tasks" | "updatetodos" | "update_todos" => {
            ToolKind::Todo
        }
        "question" | "ask" => ToolKind::Question,
        "execute" | "code" => ToolKind::Execute,
        "browser" => ToolKind::Browser,
        "image_generation" | "image" | "generate_image" => ToolKind::Image,
        _ => ToolKind::Mcp,
    }
}

fn pick_string(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(Value::String(s)) = map.get(*key) {
            if !s.is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

/// Parsed tool input, or string fields pulled from a half-written JSON object.
pub fn tool_input_record(input: &Value, partial: Option<&str>) -> Option<serde_json::Map<String, Value>> {
    match input {
        Value::Object(map) if !map.is_empty() => Some(map.clone()),
        Value::String(s) => parse_object_or_partial(s),
        Value::Null | Value::Object(_) => partial.and_then(parse_object_or_partial),
        _ => None,
    }
}

fn parse_object_or_partial(text: &str) -> Option<serde_json::Map<String, Value>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        return Some(map);
    }
    let partial = partial_json_strings(trimmed);
    if partial.is_empty() {
        None
    } else {
        Some(
            partial
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        )
    }
}

/// String fields completed so far in a half-written JSON object (App scanner).
fn partial_json_strings(text: &str) -> Vec<(String, String)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    let read_string = |index: &mut usize| -> (String, bool) {
        *index += 1;
        let mut value = String::new();
        while *index < bytes.len() {
            let char = bytes[*index] as char;
            if char == '\\' {
                let next = bytes.get(*index + 1).copied().map(|b| b as char);
                match next {
                    None => {
                        *index += 1;
                        return (value, false);
                    }
                    Some(n) => {
                        value.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                        *index += 2;
                        continue;
                    }
                }
            }
            if char == '"' {
                *index += 1;
                return (value, true);
            }
            value.push(char);
            *index += 1;
        }
        (value, false)
    };
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let (key, closed) = read_string(&mut index);
        if !closed {
            break;
        }
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
            index += 1;
        }
        if bytes.get(index) != Some(&b':') {
            continue;
        }
        index += 1;
        while index < bytes.len() && matches!(bytes[index], b' ' | b'\t') {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        if bytes[index] != b'"' {
            match text[index..].find(',') {
                Some(at) => index += at + 1,
                None => break,
            }
            continue;
        }
        let (value, closed) = read_string(&mut index);
        if !key.is_empty() {
            out.push((key, value));
        }
        if !closed {
            break;
        }
    }
    out
}

const PATH_KEYS: &[&str] = &["path", "filePath", "file_path", "file"];

/// First non-empty line. Headers are one row; hard newlines must not stack.
pub fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Shell / execute header: skip leading `#` comments when a real command
/// follows, so `# OCR…\ntesseract …` becomes the tesseract line — not two rows.
pub fn command_line(text: &str) -> String {
    let mut first = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if first.is_none() {
            first = Some(line);
        }
        if !line.starts_with('#') {
            return line.to_string();
        }
    }
    first.unwrap_or("").to_string()
}

/// One-line target the card header shows.
pub fn extract_target(kind: ToolKind, input: &Value, partial: Option<&str>) -> String {
    let Some(rec) = tool_input_record(input, partial) else {
        return String::new();
    };
    match kind {
        ToolKind::Shell => {
            command_line(&pick_string(&rec, &["command", "cmd"]).unwrap_or_default())
        }
        ToolKind::Read | ToolKind::Edit | ToolKind::Write => {
            pick_string(&rec, PATH_KEYS).unwrap_or_default()
        }
        ToolKind::Patch => {
            let text = pick_string(&rec, &["patchText", "patch_text", "patch"]);
            text.and_then(|t| first_patch_path(&t))
                .unwrap_or_else(|| pick_string(&rec, PATH_KEYS).unwrap_or_default())
        }
        ToolKind::Glob | ToolKind::Grep | ToolKind::Search => {
            pick_string(&rec, &["pattern", "query", "literal"]).unwrap_or_default()
        }
        ToolKind::Web => pick_string(&rec, &["url", "query"]).unwrap_or_default(),
        ToolKind::Subagent => pick_string(
            &rec,
            &[
                "description",
                "agent",
                "agentID",
                "agent_id",
                "subagent",
                "prompt",
            ],
        )
        .map(|s| one_line(&s))
        .unwrap_or_default(),
        ToolKind::Skill => {
            pick_string(&rec, &["id", "skillID", "skill_id", "skillId", "skill"]).unwrap_or_default()
        }
        ToolKind::Question => String::new(),
        // Cursor's `execute` often carries a shell script in `code` (comment +
        // command); also accept `command` / `cmd` the way shell tools do.
        ToolKind::Execute => command_line(
            &pick_string(&rec, &["code", "command", "cmd"]).unwrap_or_default(),
        ),
        ToolKind::Browser => pick_string(&rec, &["url", "action", "tabID"]).unwrap_or_default(),
        ToolKind::Image => {
            one_line(&pick_string(&rec, &["prompt", "description"]).unwrap_or_default())
        }
        ToolKind::Todo | ToolKind::Mcp => one_line(
            &pick_string(
                &rec,
                &[
                    "command",
                    "cmd",
                    "path",
                    "filePath",
                    "file_path",
                    "query",
                    "url",
                    "pattern",
                    "name",
                ],
            )
            .unwrap_or_default(),
        ),
    }
}

fn first_patch_path(patch: &str) -> Option<String> {
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            return Some(rest.to_string());
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            return Some(rest.trim_start_matches("b/").to_string());
        }
        if let Some(rest) = line.strip_prefix("*** Update File: ") {
            return Some(rest.to_string());
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            return Some(rest.to_string());
        }
    }
    None
}

pub fn basename(path: &str) -> String {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(path)
        .to_string()
}

pub fn dirname(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(0) => "/".into(),
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

pub fn split_url(url: &str) -> (String, String) {
    let trimmed = url.trim();
    let Some(rest) = trimmed.split_once("://").map(|(_, r)| r) else {
        return (String::new(), String::new());
    };
    let (host_part, path) = match rest.find(['/', '?', '#']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let host = host_part
        .strip_prefix("www.")
        .unwrap_or(host_part)
        .to_string();
    let path = if path == "/" {
        String::new()
    } else {
        path.to_string()
    };
    (host, path)
}

/// Header title + quieter caption.
///
/// File tools keep the path on **one** line on desktop (wide timeline). The
/// App's phone layout splits basename / folder; that wasted a second row here.
/// Shell / execute always stay a single clipped row (App `numberOfLines={1}`).
pub fn header_lines(kind: ToolKind, target: &str, title: Option<&str>) -> (String, String) {
    match kind {
        ToolKind::Read | ToolKind::Edit | ToolKind::Write | ToolKind::Patch => {
            (one_line(target), String::new())
        }
        ToolKind::Web => {
            let (host, path) = split_url(target);
            if !host.is_empty() {
                (host, path)
            } else {
                (
                    one_line(title.filter(|t| !t.is_empty()).unwrap_or(target)),
                    String::new(),
                )
            }
        }
        // Prefer the extracted command over a multiline `part.title` that
        // would otherwise reintroduce the comment + command pair.
        ToolKind::Shell | ToolKind::Execute => (command_line(target), String::new()),
        _ => {
            let chosen = title
                .filter(|t| !t.is_empty() && *t != target)
                .unwrap_or(target);
            (one_line(chosen), String::new())
        }
    }
}

#[derive(Debug, Clone)]
pub struct EditFile {
    pub path: String,
    pub patch: String,
    pub additions: u32,
    pub deletions: u32,
}

fn count_marked(patch: &str, marker: char) -> u32 {
    let header: String = std::iter::repeat(marker).take(3).collect();
    patch
        .lines()
        .filter(|l| l.starts_with(marker) && !l.starts_with(&header))
        .count() as u32
}

/// Ready-to-render unified diffs from `metadata.files` (edit / write / patch).
pub fn edit_files_from_metadata(metadata: Option<&Value>) -> Vec<EditFile> {
    let Some(Value::Object(map)) = metadata else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(Value::Array(files)) = map.get("files") {
        for entry in files {
            let Some(rec) = entry.as_object() else {
                continue;
            };
            let path = pick_string(rec, &["file", "path", "filePath", "file_path"]);
            let patch = pick_string(rec, &["patch", "diff"]);
            let (Some(path), Some(patch)) = (path, patch) else {
                continue;
            };
            let additions = rec
                .get("additions")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or_else(|| count_marked(&patch, '+'));
            let deletions = rec
                .get("deletions")
                .and_then(Value::as_u64)
                .map(|n| n as u32)
                .unwrap_or_else(|| count_marked(&patch, '-'));
            out.push(EditFile {
                path,
                patch,
                additions,
                deletions,
            });
        }
    }
    if !out.is_empty() {
        return out;
    }
    let patch = pick_string(map, &["diff", "patch"]);
    let Some(patch) = patch else {
        return out;
    };
    let path = pick_string(map, &["filepath", "filePath", "file", "path", "file_path"])
        .unwrap_or_default();
    out.push(EditFile {
        path,
        additions: count_marked(&patch, '+'),
        deletions: count_marked(&patch, '-'),
        patch,
    });
    out
}

pub fn shell_exit(metadata: Option<&Value>) -> Option<i64> {
    metadata
        .and_then(Value::as_object)
        .and_then(|m| m.get("exit"))
        .and_then(Value::as_i64)
}

pub fn shell_timed_out(metadata: Option<&Value>) -> bool {
    let Some(Value::Object(m)) = metadata else {
        return false;
    };
    m.get("timeout") == Some(&Value::Bool(true))
        || m.get("status").and_then(Value::as_str) == Some("timeout")
}

pub fn skill_name(metadata: Option<&Value>) -> Option<String> {
    metadata
        .and_then(Value::as_object)
        .and_then(|m| pick_string(m, &["name", "title"]))
}

pub fn result_count(metadata: Option<&Value>) -> Option<u64> {
    let map = metadata?.as_object()?;
    for key in ["count", "matches", "results", "total"] {
        if let Some(n) = map.get(key).and_then(Value::as_u64) {
            return Some(n);
        }
    }
    None
}

/// Fence language for a path's extension (App `fenceLanguageForPath`).
pub fn language_for_path(path: &str) -> Option<&'static str> {
    let name = basename(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "json" | "jsonc" => "json",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "md" | "markdown" | "mdx" => "markdown",
        "yml" | "yaml" => "yaml",
        "go" => "go",
        "java" => "java",
        "py" | "pyi" => "python",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "rs" => "rust",
        "sh" | "bash" | "zsh" => "bash",
        "swift" => "swift",
        "php" => "php",
        "rb" => "ruby",
        "cs" => "c-sharp",
        "toml" => "toml",
        "xml" => "xml",
        "sql" => "sql",
        _ => return None,
    })
}

/// `read` prefixes every line with its number; the fence should not repeat it.
pub fn strip_read_line_numbers(output: &str) -> String {
    if output.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = output.split('\n').collect();
    let has_header = lines
        .first()
        .is_some_and(|l| l.starts_with("Read file "));
    let body: &[&str] = if has_header { &lines[1..] } else { &lines };
    if !has_header {
        let meaningful: Vec<&str> = body
            .iter()
            .copied()
            .filter(|l| !l.trim().is_empty())
            .collect();
        if meaningful.is_empty() || !meaningful.iter().all(|l| numbering_suffix(l).is_some()) {
            return output.to_string();
        }
    }
    body.iter()
        .map(|line| numbering_suffix(line).unwrap_or(*line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn numbering_suffix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    let rest = rest.strip_prefix(':')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// Display text for a `read` result: unwrap OpenCode envelopes, drop line numbers.
pub fn prepare_read_text(output: &str, path: &str) -> String {
    if output.starts_with("Read directory ") || path.ends_with('/') {
        return output.to_string();
    }
    let unwrapped = unwrap_read_envelope(output).unwrap_or_else(|| output.to_string());
    strip_read_line_numbers(&unwrapped).trim().to_string()
}

fn unwrap_read_envelope(output: &str) -> Option<String> {
    // `<path>…</path>\n<type>file</type>\n<content>\n…\n</content>`
    let rest = output.strip_prefix("<path>")?;
    let rest = rest.split_once("</path>")?.1.trim_start_matches(['\r', '\n']);
    let rest = rest.strip_prefix("<type>")?;
    let (kind, rest) = rest.split_once("</type>")?;
    if kind.trim() != "file" {
        return None;
    }
    let rest = rest.trim_start_matches(['\r', '\n']);
    let rest = rest.strip_prefix("<content>")?;
    let rest = rest.trim_start_matches(['\r', '\n']);
    let mut text = rest
        .strip_suffix("</content>")
        .unwrap_or(rest)
        .trim_end()
        .to_string();
    if let Some(idx) = text.rfind("\n(End of file") {
        text.truncate(idx);
    }
    Some(text)
}

/// Body string for a `write` tool's input (`content` field).
pub fn write_content(input: &Value) -> Option<String> {
    let rec = tool_input_record(input, None)?;
    pick_string(&rec, &["content", "contents", "text"])
}

#[derive(Debug, Clone)]
pub struct PathGroup {
    pub directory: String,
    pub files: Vec<String>,
}

/// `glob` one-path-per-line answer, grouped by directory (App parity).
pub fn group_paths_by_directory(output: &str) -> Vec<PathGroup> {
    if output.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<PathGroup> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() || line.contains(' ') {
            continue;
        }
        let directory = dirname(line);
        let name = basename(line);
        if name.is_empty() {
            continue;
        }
        if let Some(&i) = index.get(&directory) {
            groups[i].files.push(name);
        } else {
            index.insert(directory.clone(), groups.len());
            groups.push(PathGroup {
                directory,
                files: vec![name],
            });
        }
    }
    groups
}

#[derive(Debug, Clone)]
pub struct GrepMatchLine {
    pub line: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct GrepFileMatches {
    pub file: String,
    pub lines: Vec<GrepMatchLine>,
}

/// `grep` output grouped by file (App `groupGrepMatches`).
pub fn group_grep_matches(output: &str) -> Vec<GrepFileMatches> {
    if output.is_empty() {
        return Vec::new();
    }
    let mut groups: Vec<GrepFileMatches> = Vec::new();
    let mut current: Option<usize> = None;
    for raw in output.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        if raw.starts_with("Found ") && raw.contains(" match") {
            continue;
        }
        // Indented `Line N: …`
        if let Some(rest) = raw.strip_prefix("  ").or_else(|| raw.strip_prefix("\t")) {
            if let Some(after) = rest.strip_prefix("Line ") {
                if let Some((num, text)) = after.split_once(':') {
                    if let (Ok(n), Some(i)) = (num.trim().parse::<u32>(), current) {
                        groups[i].lines.push(GrepMatchLine {
                            line: Some(n),
                            text: text.strip_prefix(' ').unwrap_or(text).to_string(),
                        });
                        continue;
                    }
                }
            }
        }
        // `path:12:text`
        if !raw.starts_with(' ') {
            if let Some((file, rest)) = raw.split_once(':') {
                if let Some((num, text)) = rest.split_once(':') {
                    if let Ok(n) = num.parse::<u32>() {
                        let fresh = match current {
                            Some(i) => groups[i].file != file,
                            None => true,
                        };
                        if fresh {
                            current = Some(groups.len());
                            groups.push(GrepFileMatches {
                                file: file.to_string(),
                                lines: Vec::new(),
                            });
                        }
                        if let Some(i) = current {
                            groups[i].lines.push(GrepMatchLine {
                                line: Some(n),
                                text: text.to_string(),
                            });
                        }
                        continue;
                    }
                }
            }
        }
        // File header `path:`
        if let Some(file) = raw.strip_suffix(':') {
            if !file.contains(' ') && !file.is_empty() {
                current = Some(groups.len());
                groups.push(GrepFileMatches {
                    file: file.to_string(),
                    lines: Vec::new(),
                });
                continue;
            }
        }
        if let Some(i) = current {
            groups[i].lines.push(GrepMatchLine {
                line: None,
                text: raw.trim().to_string(),
            });
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_the_opencode_set_and_mcp_bucket() {
        assert_eq!(classify("bash"), ToolKind::Shell);
        assert_eq!(classify("str_replace"), ToolKind::Edit);
        assert_eq!(classify("webfetch"), ToolKind::Web);
        assert_eq!(classify("todowrite"), ToolKind::Todo);
        assert_eq!(classify("updateTodos"), ToolKind::Todo);
        assert_eq!(classify("update_todos"), ToolKind::Todo);
        assert_eq!(classify("image_generation"), ToolKind::Image);
        assert_eq!(classify("my_mcp__thing"), ToolKind::Mcp);
    }

    #[test]
    fn streaming_partial_json_yields_the_command() {
        let rec = tool_input_record(&Value::Null, Some(r#"{"command": "git sta"#)).unwrap();
        assert_eq!(rec.get("command").and_then(Value::as_str), Some("git sta"));
    }

    #[test]
    fn edit_metadata_files_become_patches() {
        let meta = json!({
            "files": [{
                "file": "a.rs",
                "patch": "+++ b/a.rs\n+hi\n-old\n",
                "additions": 1,
                "deletions": 1
            }]
        });
        let files = edit_files_from_metadata(Some(&meta));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.rs");
        assert_eq!(files[0].additions, 1);
    }

    #[test]
    fn file_headers_stay_one_line() {
        let (title, cap) = header_lines(ToolKind::Read, "desktop/src/ui/a.rs", None);
        assert_eq!(title, "desktop/src/ui/a.rs");
        assert!(cap.is_empty());
    }

    #[test]
    fn shell_and_execute_headers_stay_one_line() {
        let script = "# OCR the queue screenshot for full text\ntesseract /tmp/a.png stdout -l chi_sim+eng";
        assert_eq!(
            command_line(script),
            "tesseract /tmp/a.png stdout -l chi_sim+eng"
        );
        let input = json!({ "code": script });
        let target = extract_target(ToolKind::Execute, &input, None);
        let (title, cap) = header_lines(ToolKind::Execute, &target, Some(script));
        assert_eq!(title, "tesseract /tmp/a.png stdout -l chi_sim+eng");
        assert!(cap.is_empty());
        assert!(!title.contains('\n'));

        let shell = json!({ "command": "echo hi\necho bye" });
        let target = extract_target(ToolKind::Shell, &shell, None);
        let (title, _) = header_lines(ToolKind::Shell, &target, Some("echo hi\necho bye"));
        assert_eq!(title, "echo hi");
    }

    #[test]
    fn css_path_maps_to_css_fence() {
        assert_eq!(language_for_path("desktop/src/ui/styles.css"), Some("css"));
        assert_eq!(language_for_path("a.rs"), Some("rust"));
    }

    #[test]
    fn read_line_numbers_strip_with_header() {
        let raw = "Read file a.rs\n 1: fn main() {}\n 2: // hi\n";
        assert_eq!(strip_read_line_numbers(raw), "fn main() {}\n// hi\n");
    }

    #[test]
    fn glob_paths_group_by_folder() {
        let groups = group_paths_by_directory("/a/b/one.rs\n/a/b/two.rs\n/a/c/three.rs\n");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].directory, "/a/b");
        assert_eq!(groups[0].files, vec!["one.rs", "two.rs"]);
        assert_eq!(groups[1].files, vec!["three.rs"]);
    }
}
