//! Unified normalization of tool calls (names, inputs, diffs, shell metadata,
//! media/file blocks) across all 11 agent adapters.
//!
//! Every agent speaks a slightly different native tool dialect (Codex
//! `imageView`/`fileChange`, Antigravity `edit_file`/`read_file`/`run_command`
//! with PascalCase `Path`/`Target`/`Replacement`/`CommandLine`, Claude `Read`/
//! `Edit` with Anthropic `image` blocks, Pi `read`/`edit` with `{type:"image"}`
//! and `details.diff`, ACP wrapped `content.content` and `{type:"diff"}`,
//! OpenCode, DeepSeek, T3).
//!
//! This module projects all of them into one canonical `ToolCall` contract so
//! Mobile App and Desktop render consistent native cards (diff cards, shell
//! cards, image/file preview cards, search/web/subagent cards) without raw JSON
//! dumps or per-agent UI hacks:
//! - Non-canonical tool names (`edit_file`, `replace_file_content`,
//!   `write_file`, `run_command`, `read_file`, `view_file`, `find_by_name`,
//!   `grep_search`, `search_web`, `read_url_content`, `invoke_subagent`, etc.)
//!   are canonicalized (`edit`, `write`, `shell`, `read`, `glob`, `grep`,
//!   `web_search`, `webfetch`, `subagent`) while recording `metadata.nativeTool`.
//! - PascalCase / provider-specific input keys are canonicalized (`path`,
//!   `old_string`, `new_string`, `content`, `command`, `cwd`, `pattern`, `url`,
//!   `query`, `prompt`, `description`, `agent`).
//! - Edit and write tools gain unified `metadata.files[]` diffs (`path`,
//!   `patch`, `diff`, `additions`, `deletions`, `status`) when absent.
//! - Shell exit codes in structured or prose output are lifted to `metadata.exit`.
//! - Empty `{}` / `[]` outputs are cleared so cards don't print bare `{}`.
//! - Typed image/resource blocks become canonical `{"type": "file", "uri": ...}`
//!   entries, and completed read-image calls synthesize a canonical `file` entry
//!   enforced by strict byte-sniffing under `FileScope::AccountImageView`.

use std::path::Path;

use serde_json::{json, Map, Value};

use crate::agents::domain::{ToolCall, ToolCallStatus};

/// Maximum changed-line product for exact LCS diff; larger edits fall back to
/// a bounded linear diff so memory and CPU stay tightly capped.
const MAX_LCS_CELLS: usize = 200_000;
/// Maximum diff body lines emitted into synthesized `metadata.files[].patch`.
const MAX_SYNTHETIC_PATCH_LINES: usize = 400;

/// Canonical file/image read tool names across all supported agents.
pub(crate) fn is_read_file_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "image_view" | "view_image" | "read_file" | "view_file" | "read" | "view" | "open_file"
    )
}

fn is_shell_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "shell"
            | "bash"
            | "terminal"
            | "run"
            | "exec_command"
            | "local_shell"
            | "run_command"
            | "run_shell_command"
            | "execute_command"
            | "shell_command"
            | "command_execution"
    )
}

fn is_edit_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "edit"
            | "multiedit"
            | "str_replace"
            | "str_replace_editor"
            | "notebookedit"
            | "edit_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "fs_edit"
            | "modify_file"
            | "patch_file"
    )
}

fn is_write_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "write"
            | "create"
            | "new_file"
            | "write_file"
            | "write_to_file"
            | "create_file"
            | "save_file"
    )
}

fn is_glob_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "glob" | "find" | "find_by_name" | "find_files" | "file_search" | "list_files"
    )
}

fn is_grep_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "grep"
            | "ripgrep"
            | "grep_search"
            | "grep_files"
            | "search_files"
            | "search_in_files"
            | "content_search"
    )
}

fn is_web_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "webfetch"
            | "fetch"
            | "web_fetch"
            | "fetch_url"
            | "read_url_content"
            | "websearch"
            | "web_search"
            | "search_web"
            | "internet_search"
    )
}

fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "subagent"
            | "task"
            | "spawn"
            | "agent"
            | "invoke_subagent"
            | "launch_subagent"
            | "run_subagent"
    )
}

/// Map proprietary/non-canonical tool aliases to the canonical tool vocabulary
/// understood by every Muqun client (Mobile App + Desktop). Returns `None` when
/// the tool name is already canonical or is a custom/MCP tool.
fn canonical_tool_name(name: &str, input: &Value) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "run_command" | "run_shell_command" | "execute_command" | "shell_command"
        | "command_execution" => Some("shell"),
        "execute"
            if input.as_object().is_some_and(|m| {
                m.contains_key("command") || m.contains_key("CommandLine") || m.contains_key("cmd")
            }) =>
        {
            Some("shell")
        }
        "read_file" | "view_file" | "view" | "open_file" | "view_file_outline"
        | "view_code_item" => Some("read"),
        "list_dir" | "list_directory" => Some("list"),
        "view_image" => Some("image_view"),
        "edit_file"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "fs_edit"
        | "modify_file"
        | "patch_file" => Some("edit"),
        "write_file" | "write_to_file" | "create_file" | "save_file" => Some("write"),
        "find_by_name" | "find_files" | "file_search" | "list_files" => Some("glob"),
        "grep_search" | "grep_files" | "search_files" | "search_in_files" | "content_search" => {
            Some("grep")
        }
        "search_web" | "internet_search" => Some("web_search"),
        "read_url_content" | "fetch_url" | "web_fetch" => Some("webfetch"),
        "invoke_subagent" | "launch_subagent" | "run_subagent" => Some("subagent"),
        "ask_question" => Some("question"),
        _ => None,
    }
}

/// Ordered keys where agents place a file path in tool inputs.
pub(crate) const INPUT_PATH_KEYS: &[&str] = &[
    "path",
    "filePath",
    "file_path",
    "file",
    "Path",
    "AbsolutePath",
    "TargetFile",
    "DirectoryPath",
];

fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control)
}

fn is_absolute_host_path(path: &str) -> bool {
    valid_path(path) && path.starts_with('/') && !path.starts_with("//")
}

/// Extract the structured file path from a tool call's `input` (or nested `arguments`).
pub(crate) fn input_path(input: &Value) -> Option<&str> {
    let map = input.as_object()?;
    let target = map
        .get("arguments")
        .and_then(Value::as_object)
        .filter(|args| !args.is_empty())
        .unwrap_or(map);
    INPUT_PATH_KEYS
        .iter()
        .find_map(|key| target.get(*key).and_then(Value::as_str))
        .filter(|path| valid_path(path))
}

/// Whether `path` has a recognized image file extension.
pub(crate) fn has_image_extension(path: &str) -> bool {
    let raw = path.split(['?', '#']).next().unwrap_or(path);
    matches!(
        Path::new(raw)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "bmp"
                | "tif"
                | "tiff"
                | "avif"
                | "heic"
                | "heif"
                | "ico"
                | "svg"
        )
    )
}

/// Whether `path` is a rich binary document/media file that benefits from an
/// inline file card when a read tool returns no text body (unlike ordinary
/// source code / plain text reads, which stay compact foldable `Read` rows).
pub(crate) fn has_rich_document_extension(path: &str) -> bool {
    let raw = path.split(['?', '#']).next().unwrap_or(path);
    matches!(
        Path::new(raw)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref(),
        Some("pdf" | "docx" | "xlsx" | "pptx" | "mp4" | "mov" | "webm" | "mp3" | "wav" | "m4a")
    )
}

/// Convert an absolute host path into a `file://` URI (matching Codex `imageView`),
/// leaving relative paths and existing URIs untouched.
pub(crate) fn path_to_uri(path: &str) -> Option<String> {
    if !valid_path(path) {
        return None;
    }
    if is_absolute_host_path(path) {
        reqwest::Url::from_file_path(path)
            .ok()
            .map(|uri| uri.to_string())
    } else {
        Some(path.to_owned())
    }
}

fn pick_mime(obj: &Value) -> Option<&str> {
    ["mime", "mimeType", "mediaType", "media_type"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|mime| {
            !mime.is_empty() && mime.contains('/') && !mime.chars().any(char::is_control)
        })
}

fn data_uri(mime: &str, data: &str) -> Option<String> {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("data:") {
        Some(trimmed.to_owned())
    } else {
        Some(format!("data:{mime};base64,{trimmed}"))
    }
}

fn file_block(uri: String, mime: Option<&str>) -> Value {
    match mime {
        Some(mime) => json!({ "type": "file", "uri": uri, "mime": mime }),
        None => json!({ "type": "file", "uri": uri }),
    }
}

/// Normalize one provider-specific media content block (ACP wrapped `content`,
/// Pi/MCP/DeepSeek `image`, Anthropic `image.source`, `resource_link`,
/// `resource`) into the canonical `{"type": "file", "uri": ..., "mime"?: ...}`.
///
/// Returns `None` for non-media blocks (`text`, `diff`, etc.) or blocks that
/// are already canonical `{"type": "file", "uri": ...}`.
pub(crate) fn normalize_content_block(part: &Value) -> Option<Value> {
    let obj = part.as_object()?;
    let outer_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    let (inner, wrapped) = if outer_type == "content" {
        (obj.get("content").filter(|v| v.is_object())?, true)
    } else {
        (part, false)
    };
    let kind = inner.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "file" if wrapped => {
            let uri = inner
                .get("uri")
                .and_then(Value::as_str)
                .filter(|uri| !uri.is_empty())?;
            Some(file_block(uri.to_owned(), pick_mime(inner)))
        }
        "image" => {
            if let Some(source) = inner.get("source").filter(|v| v.is_object()) {
                let mime = pick_mime(source).or_else(|| pick_mime(inner));
                if let Some(data) = source
                    .get("data")
                    .and_then(Value::as_str)
                    .filter(|d| !d.trim().is_empty())
                {
                    let mime = mime.unwrap_or("image/png");
                    return Some(file_block(data_uri(mime, data)?, Some(mime)));
                }
                if let Some(uri) = ["url", "uri"]
                    .iter()
                    .find_map(|k| source.get(*k).and_then(Value::as_str))
                    .filter(|u| !u.is_empty())
                {
                    return Some(file_block(uri.to_owned(), mime));
                }
            }
            let mime = pick_mime(inner);
            if let Some(data) = ["data", "base64"]
                .iter()
                .find_map(|k| inner.get(*k).and_then(Value::as_str))
                .filter(|d| !d.trim().is_empty())
            {
                let mime = mime.unwrap_or("image/png");
                return Some(file_block(data_uri(mime, data)?, Some(mime)));
            }
            if let Some(uri) = ["uri", "url", "path"]
                .iter()
                .find_map(|k| inner.get(*k).and_then(Value::as_str))
                .filter(|u| !u.is_empty())
            {
                let uri = if is_absolute_host_path(uri) {
                    path_to_uri(uri)?
                } else {
                    uri.to_owned()
                };
                return Some(file_block(uri, mime));
            }
            None
        }
        "resource_link" => {
            let uri = inner
                .get("uri")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())?;
            Some(file_block(uri.to_owned(), pick_mime(inner)))
        }
        "resource" => {
            let res = inner.get("resource").filter(|v| v.is_object())?;
            let mime = pick_mime(res).or_else(|| pick_mime(inner));
            if let Some(blob) = res
                .get("blob")
                .and_then(Value::as_str)
                .filter(|b| !b.trim().is_empty())
            {
                let mime = mime.unwrap_or("application/octet-stream");
                return Some(file_block(data_uri(mime, blob)?, Some(mime)));
            }
            let uri = res
                .get("uri")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())?;
            Some(file_block(uri.to_owned(), mime))
        }
        _ => None,
    }
}

fn has_file_block(content: Option<&Value>) -> bool {
    content.and_then(Value::as_array).is_some_and(|parts| {
        parts
            .iter()
            .any(|part| part["type"] == "file" && part["uri"].is_string())
    })
}

/// Whether a tool call has any substantive text output (ignoring empty strings
/// and bare file/image URIs).
pub(crate) fn has_text_output(call: &ToolCall) -> bool {
    if let Some(output) = &call.output {
        match output {
            Value::Null => {}
            Value::String(s) if s.trim().is_empty() => {}
            Value::Array(arr) if arr.is_empty() => {}
            Value::Object(map) if map.is_empty() => {}
            _ => return true,
        }
    }
    call.content
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("text")
                    .or_else(|| part.pointer("/content/text"))
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty())
            })
        })
}

/// Rename the first present alias in `aliases` to `canonical` when none of
/// `existing_canonical` already hold a non-empty value.
fn rename_key(
    map: &mut Map<String, Value>,
    existing_canonical: &[&str],
    canonical: &str,
    aliases: &[&str],
    allow_empty_string: bool,
) {
    if existing_canonical.iter().any(|k| {
        map.get(*k).is_some_and(|v| match v {
            Value::Null => false,
            Value::String(s) => allow_empty_string || !s.is_empty(),
            _ => true,
        })
    }) {
        return;
    }
    let found = aliases
        .iter()
        .find(|alias| {
            map.get(**alias).is_some_and(|v| match v {
                Value::Null => false,
                Value::String(s) => allow_empty_string || !s.is_empty(),
                _ => true,
            })
        })
        .copied();
    if let Some(old_key) = found {
        if let Some(value) = map.remove(old_key) {
            map.insert(canonical.to_owned(), value);
        }
    }
}

fn normalize_tool_input(name: &str, input: &mut Value) {
    let Some(map) = input.as_object_mut() else {
        return;
    };
    if is_read_file_tool(name) || name.eq_ignore_ascii_case("list") {
        rename_key(
            map,
            &["path", "filePath", "file_path", "file"],
            "path",
            &["Path", "AbsolutePath", "TargetFile", "DirectoryPath"],
            false,
        );
    } else if is_edit_tool(name) {
        rename_key(
            map,
            &["path", "filePath", "file_path", "file"],
            "path",
            &["Path", "AbsolutePath", "TargetFile"],
            false,
        );
        rename_key(
            map,
            &["old_string", "oldString"],
            "old_string",
            &[
                "Target",
                "TargetContent",
                "oldText",
                "old_text",
                "oldStr",
                "old_str",
                "search",
                "originalContent",
                "original_content",
            ],
            true,
        );
        rename_key(
            map,
            &["new_string", "newString"],
            "new_string",
            &[
                "Replacement",
                "ReplacementContent",
                "newText",
                "new_text",
                "newStr",
                "new_str",
                "replace",
                "newContent",
                "new_content",
            ],
            true,
        );
    } else if is_write_tool(name) {
        rename_key(
            map,
            &["path", "filePath", "file_path", "file"],
            "path",
            &["Path", "AbsolutePath", "TargetFile"],
            false,
        );
        rename_key(
            map,
            &["content"],
            "content",
            &["Content", "CodeContent", "contents", "text"],
            true,
        );
    } else if is_shell_tool(name) {
        rename_key(map, &["command"], "command", &["CommandLine", "cmd"], false);
        rename_key(
            map,
            &["cwd", "workdir"],
            "cwd",
            &["Cwd", "WorkingDirectory"],
            false,
        );
    } else if is_glob_tool(name) {
        rename_key(
            map,
            &["pattern"],
            "pattern",
            &["Pattern", "glob_pattern"],
            false,
        );
        rename_key(
            map,
            &["path", "directory"],
            "path",
            &["SearchDirectory", "DirectoryPath", "Path"],
            false,
        );
    } else if is_grep_tool(name) {
        rename_key(
            map,
            &["pattern", "query"],
            "pattern",
            &["Query", "SearchQuery", "Pattern", "literal"],
            false,
        );
        rename_key(
            map,
            &["path", "directory"],
            "path",
            &["SearchPath", "DirectoryPath", "Path"],
            false,
        );
    } else if is_web_tool(name) {
        rename_key(map, &["url"], "url", &["Url", "URI"], false);
        rename_key(map, &["query"], "query", &["Query"], false);
    } else if is_subagent_tool(name) {
        rename_key(map, &["prompt"], "prompt", &["Prompt"], false);
        rename_key(
            map,
            &["description"],
            "description",
            &["Role", "Title"],
            false,
        );
        rename_key(
            map,
            &["agent", "subagent_type", "agent_id", "agentID"],
            "agent",
            &["TypeName", "SubagentType"],
            false,
        );
    }
}

fn ensure_metadata_object(call: &mut ToolCall) -> &mut Map<String, Value> {
    if !call.metadata.as_ref().is_some_and(Value::is_object) {
        call.metadata = Some(Value::Object(Map::new()));
    }
    call.metadata
        .as_mut()
        .and_then(Value::as_object_mut)
        .expect("metadata initialized to object")
}

fn has_valid_metadata_files(metadata: Option<&Value>) -> bool {
    metadata
        .and_then(|m| m.get("files"))
        .and_then(Value::as_array)
        .is_some_and(|files| {
            files.iter().any(|entry| {
                let patch = entry
                    .get("patch")
                    .or_else(|| entry.get("diff"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                !patch.trim().is_empty()
            })
        })
}

fn count_patch_changes(patch: &str) -> (usize, usize) {
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in patch.lines() {
        if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    (additions, deletions)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffOp<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Build a standard unified diff (`--- a/{path}`, `+++ b/{path}`, `@@ ... @@`)
/// with 3 lines of context and exact `(additions, deletions)` counts.
fn unified_diff(path: &str, old: &str, new: &str, start_line: usize) -> (String, usize, usize) {
    let old_lines: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let new_lines: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        new.lines().collect()
    };

    if old_lines == new_lines {
        return (String::new(), 0, 0);
    }

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];

    let mut ops: Vec<DiffOp<'_>> = Vec::with_capacity(old_lines.len() + new_lines.len());
    for line in &old_lines[..prefix] {
        ops.push(DiffOp::Context(line));
    }

    if old_mid.len().saturating_mul(new_mid.len()) <= MAX_LCS_CELLS {
        let m = old_mid.len();
        let n = new_mid.len();
        let cols = n + 1;
        let mut dp = vec![0u32; (m + 1) * cols];
        for i in (0..m).rev() {
            for j in (0..n).rev() {
                dp[i * cols + j] = if old_mid[i] == new_mid[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < m && j < n {
            if old_mid[i] == new_mid[j] {
                ops.push(DiffOp::Context(old_mid[i]));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
                ops.push(DiffOp::Removed(old_mid[i]));
                i += 1;
            } else {
                ops.push(DiffOp::Added(new_mid[j]));
                j += 1;
            }
        }
        while i < m {
            ops.push(DiffOp::Removed(old_mid[i]));
            i += 1;
        }
        while j < n {
            ops.push(DiffOp::Added(new_mid[j]));
            j += 1;
        }
    } else {
        for line in old_mid {
            ops.push(DiffOp::Removed(line));
        }
        for line in new_mid {
            ops.push(DiffOp::Added(line));
        }
    }

    for line in &old_lines[old_lines.len() - suffix..] {
        ops.push(DiffOp::Context(line));
    }

    let additions = ops
        .iter()
        .filter(|op| matches!(op, DiffOp::Added(_)))
        .count();
    let deletions = ops
        .iter()
        .filter(|op| matches!(op, DiffOp::Removed(_)))
        .count();

    // Emit unified diff hunks with up to 3 lines of context around each change.
    const CONTEXT: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter_map(|(idx, op)| (!matches!(op, DiffOp::Context(_))).then_some(idx))
        .collect();
    if changed.is_empty() {
        return (String::new(), 0, 0);
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for idx in changed {
        let start = idx.saturating_sub(CONTEXT);
        let end = (idx + CONTEXT + 1).min(ops.len());
        if let Some(last) = ranges.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    let mut out = if old_lines.is_empty() {
        format!("--- /dev/null\n+++ b/{path}\n")
    } else if new_lines.is_empty() {
        format!("--- a/{path}\n+++ /dev/null\n")
    } else {
        format!("--- a/{path}\n+++ b/{path}\n")
    };

    let mut emitted_lines = 0usize;
    let base = start_line.max(1);
    let mut old_cursor = base;
    let mut new_cursor = base;
    let mut op_idx = 0usize;

    for (h_start, h_end) in ranges {
        while op_idx < h_start {
            match ops[op_idx] {
                DiffOp::Context(_) => {
                    old_cursor += 1;
                    new_cursor += 1;
                }
                DiffOp::Removed(_) => old_cursor += 1,
                DiffOp::Added(_) => new_cursor += 1,
            }
            op_idx += 1;
        }
        let slice = &ops[h_start..h_end];
        let old_count = slice
            .iter()
            .filter(|op| matches!(op, DiffOp::Context(_) | DiffOp::Removed(_)))
            .count();
        let new_count = slice
            .iter()
            .filter(|op| matches!(op, DiffOp::Context(_) | DiffOp::Added(_)))
            .count();
        let hunk_old_start = if old_count == 0 {
            old_cursor.saturating_sub(1)
        } else {
            old_cursor
        };
        let hunk_new_start = if new_count == 0 {
            new_cursor.saturating_sub(1)
        } else {
            new_cursor
        };
        out.push_str(&format!(
            "@@ -{hunk_old_start},{old_count} +{hunk_new_start},{new_count} @@\n"
        ));
        for op in slice {
            if emitted_lines >= MAX_SYNTHETIC_PATCH_LINES {
                break;
            }
            match op {
                DiffOp::Context(line) => {
                    out.push(' ');
                    out.push_str(line);
                    out.push('\n');
                    old_cursor += 1;
                    new_cursor += 1;
                }
                DiffOp::Removed(line) => {
                    out.push('-');
                    out.push_str(line);
                    out.push('\n');
                    old_cursor += 1;
                }
                DiffOp::Added(line) => {
                    out.push('+');
                    out.push_str(line);
                    out.push('\n');
                    new_cursor += 1;
                }
            }
            emitted_lines += 1;
        }
        op_idx = h_end;
        if emitted_lines >= MAX_SYNTHETIC_PATCH_LINES {
            break;
        }
    }

    (out, additions, deletions)
}

fn file_diff_entry(
    path: &str,
    patch: String,
    additions: usize,
    deletions: usize,
    status: &str,
) -> Value {
    json!({
        "path": path,
        "patch": patch,
        "diff": patch,
        "additions": additions,
        "deletions": deletions,
        "status": status,
    })
}

fn project_edit_write_diff(call: &mut ToolCall) {
    if has_valid_metadata_files(call.metadata.as_ref()) {
        return;
    }

    let mut files: Vec<Value> = Vec::new();

    // 1. ACP `{type: "diff", path, oldText, newText}` blocks in `call.content[]`.
    if let Some(parts) = call.content.as_ref().and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("diff") {
                continue;
            }
            let Some(path) = part
                .get("path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            if let Some(patch) = part
                .get("patch")
                .and_then(Value::as_str)
                .filter(|p| !p.trim().is_empty())
            {
                let (add, del) = count_patch_changes(patch);
                files.push(file_diff_entry(
                    path,
                    patch.to_owned(),
                    add,
                    del,
                    "modified",
                ));
            } else if part.get("oldText").is_some() || part.get("newText").is_some() {
                let old = part.get("oldText").and_then(Value::as_str).unwrap_or("");
                let new = part.get("newText").and_then(Value::as_str).unwrap_or("");
                let (patch, add, del) = unified_diff(path, old, new, 1);
                if !patch.is_empty() {
                    let status = if old.is_empty() { "added" } else { "modified" };
                    files.push(file_diff_entry(path, patch, add, del, status));
                }
            }
        }
    }

    // 2. Native unified diff already present in `metadata.diff` (Pi `details.diff`)
    //    or `output.patch` / `output.diff` (DeepSeek).
    if files.is_empty() && (is_edit_tool(&call.name) || is_write_tool(&call.name)) {
        let native_patch = call
            .metadata
            .as_ref()
            .and_then(|m| m.get("diff").or_else(|| m.get("patch")))
            .or_else(|| {
                call.output
                    .as_ref()
                    .and_then(|o| o.get("patch").or_else(|| o.get("diff")))
            })
            .and_then(Value::as_str)
            .filter(|p| {
                !p.trim().is_empty() && (p.contains("@@") || p.contains("\n+") || p.contains("\n-"))
            });
        if let (Some(path), Some(patch)) = (input_path(&call.input), native_patch) {
            let (add, del) = count_patch_changes(patch);
            let status = if is_write_tool(&call.name) {
                "added"
            } else {
                "modified"
            };
            files.push(file_diff_entry(path, patch.to_owned(), add, del, status));
        }
    }

    // 3. Structured edit input (`old_string` / `new_string`, or `ReplacementChunks[]`).
    if files.is_empty() && is_edit_tool(&call.name) {
        if let (Some(path), Some(map)) = (input_path(&call.input), call.input.as_object()) {
            let old = map
                .get("old_string")
                .or_else(|| map.get("oldString"))
                .and_then(Value::as_str);
            let new = map
                .get("new_string")
                .or_else(|| map.get("newString"))
                .and_then(Value::as_str);
            if old.is_some() || new.is_some() {
                let start_line = map
                    .get("StartLine")
                    .or_else(|| map.get("start_line"))
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(1);
                let (patch, add, del) =
                    unified_diff(path, old.unwrap_or(""), new.unwrap_or(""), start_line);
                if !patch.is_empty() {
                    files.push(file_diff_entry(path, patch, add, del, "modified"));
                }
            } else if let Some(chunks) = map.get("ReplacementChunks").and_then(Value::as_array) {
                let mut combined = String::new();
                let (mut total_add, mut total_del) = (0usize, 0usize);
                for chunk in chunks {
                    let old = chunk
                        .get("TargetContent")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let new = chunk
                        .get("ReplacementContent")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let start_line = chunk
                        .get("StartLine")
                        .and_then(Value::as_u64)
                        .map(|n| n as usize)
                        .unwrap_or(1);
                    let (patch, add, del) = unified_diff(path, old, new, start_line);
                    if !patch.is_empty() {
                        if combined.is_empty() {
                            combined.push_str(&patch);
                        } else if let Some(at) = patch.find("@@ ") {
                            combined.push_str(&patch[at..]);
                        }
                        total_add += add;
                        total_del += del;
                    }
                }
                if !combined.is_empty() {
                    files.push(file_diff_entry(
                        path, combined, total_add, total_del, "modified",
                    ));
                }
            }
        }
    }

    // 4. Structured write input (`path` + `content`).
    if files.is_empty() && is_write_tool(&call.name) {
        if let (Some(path), Some(content)) = (
            input_path(&call.input),
            call.input.get("content").and_then(Value::as_str),
        ) {
            if !content.is_empty() {
                let (patch, add, del) = unified_diff(path, "", content, 1);
                if !patch.is_empty() {
                    files.push(file_diff_entry(path, patch, add, del, "added"));
                }
            }
        }
    }

    if !files.is_empty() {
        let meta = ensure_metadata_object(call);
        meta.insert("files".to_owned(), Value::Array(files));
    }
}

fn extract_exit_code_from_text(text: &str) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("command exited with code ")?;
    let start = at + "command exited with code ".len();
    let digits: String = text[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

fn project_shell_metadata(call: &mut ToolCall) {
    if !is_shell_tool(&call.name) {
        return;
    }
    let already_has_exit = call
        .metadata
        .as_ref()
        .and_then(Value::as_object)
        .is_some_and(|m| m.get("exit").or_else(|| m.get("exit_code")).is_some());
    if already_has_exit {
        return;
    }
    let exit = match &call.output {
        Some(Value::Object(map)) => map
            .get("exit_code")
            .or_else(|| map.get("exitCode"))
            .or_else(|| map.get("exit"))
            .and_then(Value::as_i64),
        Some(Value::String(s)) => extract_exit_code_from_text(s),
        _ => None,
    };
    if let Some(code) = exit {
        let meta = ensure_metadata_object(call);
        meta.insert("exit".to_owned(), json!(code));
    }
}

fn strip_empty_output(call: &mut ToolCall) {
    let empty = match &call.output {
        Some(Value::Object(map)) => map.is_empty(),
        Some(Value::Array(arr)) => arr.is_empty(),
        _ => false,
    };
    if empty {
        call.output = None;
    }
}

/// Normalize a `ToolCall`'s name, inputs, diffs, shell metadata, empty outputs,
/// and media/file content blocks into the unified Gateway contract.
pub(crate) fn normalize_tool_call(call: &mut ToolCall) {
    // 1. Canonicalize non-standard tool names (preserving `metadata.nativeTool`).
    if let Some(canonical) = canonical_tool_name(&call.name, &call.input) {
        let native = std::mem::replace(&mut call.name, canonical.to_owned());
        let meta = ensure_metadata_object(call);
        meta.entry("nativeTool".to_owned())
            .or_insert(Value::String(native));
    }

    // 2. Canonicalize PascalCase / provider-specific input keys.
    normalize_tool_input(&call.name, &mut call.input);

    // 3. Drop bare empty `{}` / `[]` outputs (e.g. Antigravity `edit_file`) and
    //    noisy `"File <relpath>"` titles when structured `input.path` is present.
    strip_empty_output(call);
    if (is_read_file_tool(&call.name) || is_edit_tool(&call.name) || is_write_tool(&call.name))
        && input_path(&call.input).is_some()
        && call
            .title
            .as_deref()
            .is_some_and(|t| t.starts_with("File "))
    {
        call.title = None;
    }

    // 4. Project unified `metadata.files` diffs for edit/write tools and
    //    `metadata.exit` for shell tools.
    project_edit_write_diff(call);
    project_shell_metadata(call);

    // 5. Normalize provider-specific image/resource blocks inside `content[]`.
    if let Some(Value::Array(parts)) = &mut call.content {
        for part in parts.iter_mut() {
            if let Some(normalized) = normalize_content_block(part) {
                *part = normalized;
            }
        }
    }

    // 6. Extract provider-specific image/resource blocks from array `output`
    //    when `content` does not already carry typed file blocks (e.g. T3/MCP).
    if !has_file_block(call.content.as_ref()) {
        if let Some(Value::Array(parts)) = &call.output {
            let extracted: Vec<Value> = parts.iter().filter_map(normalize_content_block).collect();
            if !extracted.is_empty() {
                match &mut call.content {
                    Some(Value::Array(existing)) => existing.extend(extracted),
                    _ => call.content = Some(Value::Array(extracted)),
                }
            }
        }
    }

    // 7. Synthesize a canonical `{"type": "file", "uri": ...}` block for any
    //    completed read/view tool call whose structured input points at an
    //    image file and whose adapter did not emit a typed file block.
    if is_read_file_tool(&call.name)
        && call.state == ToolCallStatus::Completed
        && !has_file_block(call.content.as_ref())
    {
        if let Some(path) = input_path(&call.input).filter(|p| has_image_extension(p)) {
            if let Some(uri) = path_to_uri(path) {
                let block = json!({ "type": "file", "uri": uri });
                match &mut call.content {
                    Some(Value::Array(existing)) => existing.push(block),
                    _ => call.content = Some(Value::Array(vec![block])),
                }
            }
        }
    }
}

/// The exact `file://` URI authorized for `FileScope::AccountImageView` on a
/// canonical image-view / read-image tool call.
///
/// Security invariants preserved:
/// - Unrelated tools (`unrelated_tool`, `any_provider_tool`, `shell`, etc.)
///   always return `None` and stay strictly fenced to `FileScope::Session`.
/// - Codex's structured `imageView` (`metadata.type == "imageView"`) keeps its
///   exact matching policy.
/// - General read/view tools (`read`, `read_file`, `view_file`, `view`,
///   `view_image`) qualify only when their structured input path has an image
///   extension, and `originals::normalize` additionally verifies the sniffed
///   magic bytes are `AssetKind::Image` (refusing any non-image file with
///   `image_content_unavailable` and `download_url: None`).
pub(crate) fn viewed_image_uri(call: &ToolCall) -> Option<String> {
    if !is_read_file_tool(&call.name) {
        return None;
    }
    if call.name == "image_view"
        && call
            .metadata
            .as_ref()
            .is_some_and(|m| m["type"] == "imageView" && m["path"] == call.input["path"])
    {
        return call.input["path"]
            .as_str()
            .and_then(|p| reqwest::Url::from_file_path(p).ok())
            .map(|u| u.to_string());
    }
    let path = input_path(&call.input)?;
    if !has_image_extension(path) {
        return None;
    }
    path_to_uri(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::ToolTime;

    fn make_call(
        name: &str,
        input: Value,
        output: Option<Value>,
        content: Option<Value>,
        state: ToolCallStatus,
    ) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            title: None,
            input,
            output,
            content,
            metadata: None,
            state,
            status: state,
            error: None,
            child_session_id: None,
            background: false,
            input_partial: None,
            truncated: false,
            time: ToolTime::default(),
        }
    }

    #[test]
    fn antigravity_read_file_png_normalizes_name_path_and_synthesizes_file_block() {
        let mut call = make_call(
            "read_file",
            json!({ "Path": "/home/demo/.gemini/scratch/shot.png" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut call);
        assert_eq!(call.name, "read");
        assert_eq!(
            call.metadata.as_ref().unwrap()["nativeTool"],
            json!("read_file")
        );
        assert_eq!(
            call.input,
            json!({ "path": "/home/demo/.gemini/scratch/shot.png" })
        );
        assert_eq!(
            call.content,
            Some(json!([{ "type": "file", "uri": "file:///home/demo/.gemini/scratch/shot.png" }]))
        );
        assert_eq!(
            viewed_image_uri(&call).as_deref(),
            Some("file:///home/demo/.gemini/scratch/shot.png")
        );
    }

    #[test]
    fn antigravity_edit_file_normalizes_to_edit_with_unified_diff_and_strips_empty_output() {
        let mut call = make_call(
            "edit_file",
            json!({
                "Path": "/w/src/lib.rs",
                "Target": "let x = match opt {\n    Some(v) => v,\n    None => return None,\n};",
                "Replacement": "let x = opt?;"
            }),
            Some(json!({})),
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut call);
        assert_eq!(call.name, "edit");
        assert!(call.output.is_none(), "empty {{}} output must be cleared");
        assert_eq!(call.input["path"], json!("/w/src/lib.rs"));
        assert!(call.input.get("Path").is_none());
        assert!(call.input.get("old_string").is_some());
        assert!(call.input.get("new_string").is_some());
        let files = call.metadata.as_ref().unwrap()["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], json!("/w/src/lib.rs"));
        assert_eq!(files[0]["additions"], json!(1));
        assert_eq!(files[0]["deletions"], json!(4));
        let patch = files[0]["patch"].as_str().unwrap();
        assert!(patch.contains("@@ -1,4 +1,1 @@"));
        assert!(patch.contains("-let x = match opt {"));
        assert!(patch.contains("+let x = opt?;"));
    }

    #[test]
    fn antigravity_write_file_and_run_command_and_search_tools_canonicalize() {
        let mut write = make_call(
            "write_file",
            json!({ "Path": "/w/new.rs", "Content": "fn a() {}\nfn b() {}\n" }),
            Some(json!({})),
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut write);
        assert_eq!(write.name, "write");
        assert!(write.output.is_none());
        assert_eq!(write.input["path"], json!("/w/new.rs"));
        assert_eq!(
            write.metadata.as_ref().unwrap()["files"][0]["additions"],
            json!(2)
        );
        assert_eq!(
            write.metadata.as_ref().unwrap()["files"][0]["status"],
            json!("added")
        );

        let mut shell = make_call(
            "run_command",
            json!({ "CommandLine": "cargo test", "Cwd": "/w" }),
            Some(json!({ "exit_code": 1, "output": "failed" })),
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut shell);
        assert_eq!(shell.name, "shell");
        assert_eq!(shell.input["command"], json!("cargo test"));
        assert_eq!(shell.input["cwd"], json!("/w"));
        assert_eq!(shell.metadata.as_ref().unwrap()["exit"], json!(1));

        let mut grep = make_call(
            "grep_search",
            json!({ "Query": "ToolCall", "SearchPath": "/w/src" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut grep);
        assert_eq!(grep.name, "grep");
        assert_eq!(grep.input["pattern"], json!("ToolCall"));
        assert_eq!(grep.input["path"], json!("/w/src"));

        let mut web = make_call(
            "search_web",
            json!({ "Query": "rust lcs" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut web);
        assert_eq!(web.name, "web_search");
        assert_eq!(web.input["query"], json!("rust lcs"));

        let mut sub = make_call(
            "invoke_subagent",
            json!({ "Prompt": "Audit", "Role": "Auditor", "TypeName": "research" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut sub);
        assert_eq!(sub.name, "subagent");
        assert_eq!(sub.input["prompt"], json!("Audit"));
        assert_eq!(sub.input["description"], json!("Auditor"));
        assert_eq!(sub.input["agent"], json!("research"));
    }

    #[test]
    fn acp_diff_content_projects_into_metadata_files() {
        let mut call = make_call(
            "Edit",
            json!({ "file_path": "/w/a.ts" }),
            None,
            Some(json!([
                { "type": "diff", "path": "/w/a.ts", "oldText": "a\nb\nc\n", "newText": "a\nB\nc\n" }
            ])),
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut call);
        assert_eq!(call.name, "Edit");
        let files = call.metadata.as_ref().unwrap()["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["additions"], json!(1));
        assert_eq!(files[0]["deletions"], json!(1));
        let patch = files[0]["patch"].as_str().unwrap();
        assert!(patch.contains(" a\n-b\n+B\n c\n"));
    }

    #[test]
    fn acp_pi_and_anthropic_image_blocks_normalize_to_canonical_file() {
        let mut call = make_call(
            "Read",
            json!({ "file_path": "/w/a.png" }),
            None,
            Some(json!([
                { "type": "content", "content": { "type": "text", "text": "ok" } },
                { "type": "content", "content": { "type": "image", "data": "QUJD", "mimeType": "image/png" } },
                { "type": "image", "source": { "type": "base64", "media_type": "image/webp", "data": "WFlZ" } },
                { "type": "content", "content": { "type": "resource_link", "uri": "file:///w/doc.pdf", "mimeType": "application/pdf" } },
                { "type": "diff", "path": "a.rs", "oldText": "a", "newText": "b" }
            ])),
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut call);
        let content = call.content.unwrap();
        assert_eq!(content[0]["content"]["text"], "ok");
        assert_eq!(
            content[1],
            json!({ "type": "file", "uri": "data:image/png;base64,QUJD", "mime": "image/png" })
        );
        assert_eq!(
            content[2],
            json!({ "type": "file", "uri": "data:image/webp;base64,WFlZ", "mime": "image/webp" })
        );
        assert_eq!(
            content[3],
            json!({ "type": "file", "uri": "file:///w/doc.pdf", "mime": "application/pdf" })
        );
        assert_eq!(content[4]["type"], "diff");
    }

    #[test]
    fn non_read_tools_and_non_image_reads_never_get_account_image_view_scope() {
        let unrelated = make_call(
            "unrelated_tool",
            json!({ "path": "/tmp/a.png" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        assert!(viewed_image_uri(&unrelated).is_none());

        let code_read = make_call(
            "read_file",
            json!({ "Path": "/etc/passwd" }),
            None,
            None,
            ToolCallStatus::Completed,
        );
        assert!(viewed_image_uri(&code_read).is_none());
    }
}
