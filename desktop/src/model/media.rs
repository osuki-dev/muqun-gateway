//! Pictures a timeline row names that are not files on this computer: a
//! pasted image only the gateway holds (`/api/agent-images/{id}/…`, which is
//! how Codex's `[Image #1]` reaches us), and a tool's inline base64 image.
//! Both land as files in one cache folder, so the timeline draws every
//! picture from a path.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use base64::Engine;
use muqun_gateway::agents::{MessageImageAsset, TimelineItem, ToolCall};
use serde_json::Value;

/// The cache folder's name under the state directory.
pub const DIR: &str = "timeline-media";

fn dir() -> Option<&'static Path> {
    static DIR_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR_PATH
        .get_or_init(|| {
            if cfg!(test) {
                return Some(std::env::temp_dir().join("muqun-desktop-test-media"));
            }
            let (_, state) = crate::platform::directories().ok()?;
            Some(state.join(DIR))
        })
        .as_deref()
}

/// `/api/agent-images/{id}/{variant}[/{version}]` or `/api/assets/{id}/…`
/// as a file name; `None` for anything that is not a gateway media route.
fn file_name(src: &str) -> Option<String> {
    let path = src.split(['?', '#']).next()?;
    let rest = path
        .strip_prefix("/api/agent-images/")
        .or_else(|| path.strip_prefix("/api/assets/"))?;
    let name: String = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    (!name.is_empty() && !name.starts_with('.')).then_some(name)
}

/// The cached copy of a gateway media URL, when it has been fetched.
pub fn cached(src: &str) -> Option<PathBuf> {
    let path = dir()?.join(file_name(src)?);
    path.is_file().then_some(path)
}

/// Where a fetched gateway media URL goes.
pub fn target(src: &str) -> Option<PathBuf> {
    Some(dir()?.join(file_name(src)?))
}

fn local_file(src: &str) -> bool {
    let raw = src.strip_prefix("file://").unwrap_or(src);
    Path::new(raw).is_absolute() && Path::new(raw).is_file()
}

/// The one URL to fetch for an asset that is not a file here: its preview
/// (sharp enough for the thumbnail and the zoom), else the original.
fn wanted_url(asset: &MessageImageAsset) -> Option<&str> {
    if asset.unavailable_reason.is_some() || !asset.mime.starts_with("image/") {
        return None;
    }
    if [asset.src.as_str(), asset.url.as_str()]
        .iter()
        .any(|s| local_file(s))
    {
        return None;
    }
    [
        asset.preview_url.as_deref(),
        Some(asset.url.as_str()),
        Some(asset.src.as_str()),
        asset.thumbnail_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|u| file_name(u).is_some())
}

/// The gateway URLs `items` show that are neither files here nor cached.
pub fn missing(items: &[TimelineItem]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let assets = item.image_assets.iter().flatten();
        // Bare attachments only where the gateway described no assets.
        let attached = item
            .attachments
            .iter()
            .flatten()
            .filter(|a| item.image_assets.is_none() && file_name(a).is_some());
        let tool_refs = match &item.part {
            muqun_gateway::agents::AgentPart::Tool(call) => tool_image_refs(call)
                .into_iter()
                .filter(|r| file_name(r).is_some())
                .collect(),
            _ => Vec::new(),
        };
        let urls = assets
            .filter_map(wanted_url)
            .chain(attached.map(String::as_str))
            .chain(tool_refs.iter().map(String::as_str));
        for url in urls {
            if cached(url).is_none() && !out.iter().any(|u| u == url) {
                out.push(url.to_string());
            }
        }
    }
    out
}

fn image_path(path: &str) -> bool {
    matches!(
        Path::new(path.split('?').next().unwrap_or(path))
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

/// Where a tool's pictures are, as its row says (no file is touched): image
/// items of `content[]` (ACP nests them under `content`), and the picture a
/// read-like tool opened (App `readImagePath`).
pub fn tool_image_refs(call: &ToolCall) -> Vec<String> {
    use crate::model::tool_view as tv;
    let mut refs: Vec<String> = Vec::new();
    for part in call
        .content
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let media = part
            .get("content")
            .filter(|inner| inner.is_object())
            .unwrap_or(part);
        let Some(uri) = ["uri", "url", "path"]
            .iter()
            .find_map(|k| media.get(*k).and_then(Value::as_str))
        else {
            continue;
        };
        let is_image = ["mime", "mimeType", "mediaType"]
            .iter()
            .filter_map(|k| media.get(*k).and_then(Value::as_str))
            .any(|m| m.starts_with("image/"))
            || media.get("type").and_then(Value::as_str) == Some("image")
            || part.get("type").and_then(Value::as_str) == Some("image")
            || image_path(uri);
        if is_image && !refs.iter().any(|r| r == uri) {
            refs.push(uri.to_string());
        }
    }
    let lower = call.name.to_ascii_lowercase();
    let read_like =
        tv::classify(&call.name) == tv::ToolKind::Read || lower == "view" || lower == "view_image";
    if refs.is_empty() && read_like {
        let target = tv::target(tv::ToolKind::Read, tv::input_record(call).as_ref());
        if image_path(&target) {
            refs.push(target);
        }
    }
    refs
}

/// Keep a tool's inline base64 picture as a file; its path, for the row to
/// carry instead of the bytes. Blocking (decodes and writes once per picture).
pub fn store_inline(data: &str, mime: &str) -> Option<PathBuf> {
    let ext = match mime.split(';').next()?.trim() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => return None,
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    data.hash(&mut hasher);
    let path = dir()?.join(format!("inline-{:016x}.{ext}", hasher.finish()));
    if path.is_file() {
        return Some(path);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim().as_bytes())
        .ok()?;
    write(&path, &bytes).then_some(path)
}

/// Write `bytes` to `path` whole (a temp file renamed into place).
pub fn write(path: &Path, bytes: &[u8]) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_gateway_media_routes_map_to_cache_files() {
        assert_eq!(
            file_name("/api/agent-images/ai_abc/preview").as_deref(),
            Some("ai_abc-preview")
        );
        assert_eq!(
            file_name("/api/assets/as_1/content?x=1").as_deref(),
            Some("as_1-content")
        );
        assert_eq!(
            file_name("/api/agent-images/../../etc/passwd").as_deref(),
            None
        );
        assert_eq!(file_name("https://example.com/a.png"), None);
        assert_eq!(file_name("/home/demo/a.png"), None);
    }

    #[test]
    fn a_pasted_codex_image_is_fetched_by_its_preview() {
        let item: TimelineItem = serde_json::from_value(serde_json::json!({
            "id": "i", "message_id": "m", "role": "user", "seq": 1, "updated_ms": 0, "ordinal": 0,
            "part": { "type": "text", "text": "[Image #1] what is this" },
            "attachments": ["/api/agent-images/ai_1/original"],
            "image_assets": [{
                "asset_id": "ai_1", "mime": "image/png",
                "src": "/api/agent-images/ai_1/original", "url": "/api/agent-images/ai_1/original",
                "preview_url": "/api/agent-images/ai_1/preview",
                "thumbnail_url": "/api/agent-images/ai_1/thumbnail",
            }, {
                "asset_id": "", "mime": "application/octet-stream", "src": "/x.webp", "url": "",
                "unavailable_reason": "reference_not_authorized_or_unavailable",
            }],
        }))
        .unwrap();
        assert_eq!(missing(&[item]), ["/api/agent-images/ai_1/preview"]);
    }
}
