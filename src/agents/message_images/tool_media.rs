//! Unified normalization of file/image tool calls across all agent adapters.
//!
//! Every agent reports file/image reads differently (Codex `image_view`,
//! Antigravity `read_file` with `Path`, Claude `Read` with Anthropic `image`
//! blocks, Pi `read` with `{type:"image", data, mimeType}`, ACP wrapped
//! `{type:"content", content:{type:"image"|"resource_link"|"resource"}}`,
//! OpenCode `read`, DeepSeek `read`, T3 `dynamic_tool`).
//!
//! This module projects all of them into one canonical contract on `ToolCall`:
//! - Structured path input uses `"path"` when only PascalCase keys (`Path`,
//!   `AbsolutePath`, `TargetFile`) were sent.
//! - Typed image/resource blocks in `content[]` (or array `output`) become
//!   canonical `{"type": "file", "uri": "...", "mime"?: "..."}` entries so
//!   `originals::attach` strips raw base64, sniffs actual bytes, and mints
//!   authenticated `/api/agent-images/{id}/...` preview/download assets.
//! - Completed canonical read/view calls pointing at an image path get a
//!   canonical `{"type": "file", "uri": ...}` entry when the adapter emitted
//!   none, while `originals::attach` enforces strict byte-sniffed image
//!   verification under `FileScope::AccountImageView`.

use std::path::Path;

use serde_json::{json, Value};

use crate::agents::domain::{ToolCall, ToolCallStatus};

/// Canonical file/image read tool names across all supported agents.
pub(crate) fn is_read_file_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "image_view" | "view_image" | "read_file" | "view_file" | "read" | "view"
    )
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
];

const LOWER_PATH_KEYS: &[&str] = &["path", "filePath", "file_path", "file"];
const PASCAL_PATH_KEYS: &[&str] = &["Path", "AbsolutePath", "TargetFile"];

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

/// Normalize a `ToolCall`'s media content blocks and read-file input shape in place.
pub(crate) fn normalize_tool_call(call: &mut ToolCall) {
    // 1. Canonicalize PascalCase path keys (e.g. Antigravity `Path` / `TargetFile` /
    //    `AbsolutePath`) to `"path"` on read/view tools so Mobile App and Desktop
    //    both extract the target instead of falling back to raw JSON input dumps.
    if is_read_file_tool(&call.name) {
        if let Some(map) = call.input.as_object_mut() {
            let has_lower = LOWER_PATH_KEYS
                .iter()
                .any(|k| map.get(*k).and_then(Value::as_str).is_some_and(valid_path));
            if !has_lower {
                let pascal = PASCAL_PATH_KEYS.iter().find_map(|k| {
                    map.get(*k)
                        .and_then(Value::as_str)
                        .filter(|v| valid_path(v))
                        .map(|v| (*k, v.to_owned()))
                });
                if let Some((old_key, path)) = pascal {
                    map.remove(old_key);
                    map.insert("path".to_owned(), Value::String(path));
                }
            }
        }
    }

    // 2. Normalize provider-specific image/resource blocks inside `content[]`.
    if let Some(Value::Array(parts)) = &mut call.content {
        for part in parts.iter_mut() {
            if let Some(normalized) = normalize_content_block(part) {
                *part = normalized;
            }
        }
    }

    // 3. Extract provider-specific image/resource blocks from array `output`
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

    // 4. Synthesize a canonical `{"type": "file", "uri": ...}` block for any
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
/// - General read/view tools (`read_file`, `view_file`, `read`, `view`,
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
        content: Option<Value>,
        state: ToolCallStatus,
    ) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            title: None,
            input,
            output: None,
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
    fn antigravity_read_file_png_normalizes_path_and_synthesizes_file_block() {
        let mut call = make_call(
            "read_file",
            json!({ "Path": "/home/demo/.gemini/scratch/shot.png" }),
            None,
            ToolCallStatus::Completed,
        );
        normalize_tool_call(&mut call);
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
    fn acp_pi_and_anthropic_image_blocks_normalize_to_canonical_file() {
        let mut call = make_call(
            "Read",
            json!({ "file_path": "/w/a.png" }),
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
            ToolCallStatus::Completed,
        );
        assert!(viewed_image_uri(&unrelated).is_none());

        let code_read = make_call(
            "read_file",
            json!({ "Path": "/etc/passwd" }),
            None,
            ToolCallStatus::Completed,
        );
        assert!(viewed_image_uri(&code_read).is_none());
    }
}
