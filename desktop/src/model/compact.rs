//! Slim timeline rows before they reach the UI thread.
//!
//! A long Cursor or Antigravity session answers with tens of megabytes: every
//! edit carries the whole file twice (ACP's `{type: "diff", oldText,
//! newText}`), and Claude's image reads carry their base64 twice (output and
//! content). The timeline draws neither: an edit is drawn as its hunks, a
//! picture from its file. So on a runtime worker, before the rows are queued,
//! each whole-file pair becomes the unified patch of what changed
//! (`{type: "diff", path, patch, additions, deletions}`), and inline image
//! bytes are dropped. Held per open session, this is most of its memory.

use muqun_gateway::agents::{AgentPart, TimelineItem, ToolCall};
use rune_kit::diff::{FileDiff, LineKind};
use serde_json::{json, Value};

/// Inline image bytes above this are dropped (a thumbnail would be smaller).
const INLINE_IMAGE_BYTES: usize = 1024;

/// The unified patch of `old` → `new` at `path`, three lines of context.
pub fn unified_patch(path: &str, old: &str, new: &str) -> (String, usize, usize) {
    let file = FileDiff::from_texts(path, old, new);
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for hunk in &file.hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
        ));
        for line in &hunk.lines {
            out.push(match line.kind {
                LineKind::Context => ' ',
                LineKind::Added => '+',
                LineKind::Removed => '-',
            });
            out.push_str(&line.text);
            out.push('\n');
        }
    }
    (out, file.additions(), file.deletions())
}

/// A content item, slimmed: whole-file diffs as patches, images without bytes.
fn slim_block(block: &mut Value) {
    let Some(rec) = block.as_object_mut() else {
        return;
    };
    // ACP nests the real block under `content`.
    if let Some(inner) = rec.get_mut("content").filter(|c| c.is_object()) {
        slim_block(inner);
    }
    let Some(rec) = block.as_object_mut() else {
        return;
    };
    if rec.get("type").and_then(Value::as_str) == Some("diff") && rec.contains_key("newText") {
        let path = rec
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old = rec.get("oldText").and_then(Value::as_str).unwrap_or("");
        let new = rec.get("newText").and_then(Value::as_str).unwrap_or("");
        let (patch, additions, deletions) = unified_patch(&path, old, new);
        *block = json!({
            "type": "diff", "path": path, "patch": patch,
            "additions": additions, "deletions": deletions,
        });
        return;
    }
    let mime = ["mimeType", "mime", "media_type", "mediaType"]
        .iter()
        .find_map(|k| rec.get(*k).and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    for key in ["data", "base64"] {
        let Some(data) = rec
            .get(key)
            .and_then(Value::as_str)
            .filter(|d| d.len() > INLINE_IMAGE_BYTES)
        else {
            continue;
        };
        // The picture is kept as a file the row points at.
        if !rec.contains_key("uri") {
            if let Some(path) = super::media::store_inline(data, &mime) {
                rec.insert(
                    "uri".into(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
            }
        }
        rec.insert(key.into(), Value::String(String::new()));
        rec.insert("omitted".into(), Value::Bool(true));
    }
    // Anthropic's `{type: "image", source: {data}}`: the file goes on the block.
    let stored = rec.get_mut("source").and_then(|source| {
        slim_block(source);
        source.get("uri").cloned().map(|uri| (uri, mime_of(source)))
    });
    if let Some((uri, mime)) = stored {
        rec.entry("uri").or_insert(uri);
        rec.entry("mimeType").or_insert(Value::String(mime));
    }
}

fn mime_of(source: &Value) -> String {
    ["media_type", "mimeType", "mime"]
        .iter()
        .find_map(|k| source.get(*k).and_then(Value::as_str))
        .unwrap_or("image/png")
        .to_string()
}

fn slim_list(value: &mut Value) {
    if let Some(list) = value.as_array_mut() {
        for block in list {
            slim_block(block);
        }
    }
}

fn slim_call(call: &mut ToolCall) {
    if let Some(content) = call.content.as_mut() {
        slim_list(content);
    }
    if let Some(output) = call.output.as_mut() {
        slim_list(output);
    }
}

/// Slim every row of `items` in place.
pub fn slim(items: &mut [TimelineItem]) {
    for item in items {
        if let AgentPart::Tool(call) = &mut item.part {
            slim_call(call);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_file_diffs_become_patches_and_image_bytes_go() {
        let old: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 100\n", "line one hundred\n");
        let mut call: ToolCall = serde_json::from_value(json!({
            "id": "c", "name": "edit", "input": {}, "state": "completed", "status": "completed",
            "content": [
                {"type": "diff", "path": "a.txt", "oldText": old, "newText": new},
                {"type": "content", "content": {"type": "image", "data": "x".repeat(5000), "mimeType": "image/png"}},
            ],
        }))
        .unwrap();
        slim_call(&mut call);
        let content = call.content.unwrap();
        let patch = content[0]["patch"].as_str().unwrap();
        assert!(patch.contains("-line 100\n+line one hundred\n"));
        assert!(patch.len() < 400, "context only, not the file");
        assert_eq!(
            (
                content[0]["additions"].as_u64(),
                content[0]["deletions"].as_u64()
            ),
            (Some(1), Some(1))
        );
        assert_eq!(content[1]["content"]["data"], "");
    }
}
