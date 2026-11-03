//! Inline images and video in the timeline: large previews, theme hairlines,
//! open-in-OS for video. Paths resolve through
//! [`muqun_gateway::agents::message_images::resolve_host_media`].

use std::path::{Path, PathBuf};

use blinc_layout::prelude::*;
use muqun_gateway::agents::message_images::resolve_host_media;
use muqun_gateway::agents::MessageImageAsset;

use super::{meta, mono, PartCtx};
use crate::platform::shell::open_path;

/// Cap for a single inline image / video frame in the column.
const MAX_MEDIA_W: f32 = 420.0;
const MAX_MEDIA_H: f32 = 280.0;
/// Compact strip thumb when several images share a row.
const STRIP_THUMB: f32 = 96.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    File,
}

pub fn kind_for(path: &str, mime: Option<&str>) -> MediaKind {
    if let Some(m) = mime {
        if m.starts_with("image/") {
            return MediaKind::Image;
        }
        if m.starts_with("video/") {
            return MediaKind::Video;
        }
    }
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => MediaKind::Image,
        "mp4" | "mov" | "m4v" | "webm" | "mkv" => MediaKind::Video,
        _ => MediaKind::File,
    }
}

fn file_label(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

fn fit_size(w: Option<u32>, h: Option<u32>, max_w: f32, max_h: f32) -> (f32, f32) {
    let (ow, oh) = match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32, h as f32),
        _ => (max_w, max_h * 0.66),
    };
    let scale = (max_w / ow).min(max_h / oh).min(1.0);
    (
        (ow * scale).round().max(48.0),
        (oh * scale).round().max(48.0),
    )
}

fn session_cwd(ctx: &PartCtx<'_>) -> Option<String> {
    ctx.app
        .bindings
        .header
        .get()
        .and_then(|h| h.directory.clone())
}

type ResolveKey = (String, String);
type ResolveCache = std::collections::HashMap<ResolveKey, Option<PathBuf>>;

/// Path resolution hits the filesystem; remember hits across row rebuilds.
fn resolve(src: &str, cwd: Option<&str>) -> Option<PathBuf> {
    use std::sync::{Mutex, OnceLock};
    const CAP: usize = 256;
    static CACHE: OnceLock<Mutex<ResolveCache>> = OnceLock::new();
    let key = (src.to_string(), cwd.unwrap_or("").to_string());
    let cache = CACHE.get_or_init(|| Mutex::new(ResolveCache::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }
    let path = resolve_host_media(src, cwd);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= CAP {
            guard.clear();
        }
        guard.insert(key, path.clone());
    }
    path
}

fn chip(label: String) -> Div {
    div()
        .class("mq-attach-chip")
        .flex_shrink_0()
        .child(mono(label).no_cursor())
}

fn image_frame(path: PathBuf, w: f32, h: f32, openable: bool) -> Div {
    let shown = path.display().to_string();
    let mut frame = div()
        .class("mq-media")
        .class("mq-media--image")
        .w(w)
        .h(h)
        .overflow_clip()
        .flex_shrink_0()
        .child(image(shown.clone()).size(w, h).contain());
    if openable {
        frame = frame.cursor_pointer().on_click(move |_| open_path(&path));
    }
    frame
}

fn video_frame(path: PathBuf, w: f32, h: f32, label: String) -> Div {
    let open = path.clone();
    div()
        .class("mq-media")
        .class("mq-media--video")
        .w(w)
        .h(h)
        .flex_shrink_0()
        .flex_col()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(
            div()
                .class("mq-media-play")
                .child(mono("▶").size(22.0).no_cursor()),
        )
        .child(meta(label).class("mq-text-subtle").no_cursor())
        .on_click(move |_| open_path(&open))
}

/// One attachment / asset as a preview tile or chip.
pub fn tile(
    src: &str,
    cwd: Option<&str>,
    mime: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    max_w: f32,
    max_h: f32,
) -> Div {
    let kind = kind_for(src, mime);
    match resolve(src, cwd) {
        Some(path) if kind == MediaKind::Image => {
            let (w, h) = fit_size(width, height, max_w, max_h);
            image_frame(path, w, h, true)
        }
        Some(path) if kind == MediaKind::Video => {
            let (w, h) = fit_size(width.or(Some(16)), height.or(Some(9)), max_w, max_h);
            video_frame(path, w, h, file_label(src))
        }
        Some(path) => {
            let open = path.clone();
            div()
                .class("mq-attach-chip")
                .class("mq-media-file")
                .flex_shrink_0()
                .cursor_pointer()
                .child(mono(file_label(src)).no_cursor())
                .on_click(move |_| open_path(&open))
        }
        None if kind == MediaKind::Image || kind == MediaKind::Video => chip(file_label(src)),
        None => chip(file_label(src)),
    }
}

/// Attachment paths under a user / assistant row.
pub fn attachment_row(
    ctx: &PartCtx<'_>,
    paths: &[String],
    assets: Option<&[MessageImageAsset]>,
) -> Div {
    let cwd = session_cwd(ctx);
    let max_w = ctx.width.min(MAX_MEDIA_W);
    let many = paths.len() > 1;
    let thumb_w = if many { STRIP_THUMB } else { max_w };
    let thumb_h = if many { STRIP_THUMB } else { MAX_MEDIA_H };
    let mut row = div()
        .class("mq-attach-row")
        .class(if many {
            "mq-attach-row--strip"
        } else {
            "mq-attach-row--hero"
        })
        .w_full()
        .flex_row()
        .flex_wrap();
    for path in paths {
        let asset = assets.and_then(|list| {
            list.iter()
                .find(|a| a.src == *path || a.url == *path || a.asset_id == *path)
        });
        let src = asset
            .map(|a| {
                a.thumbnail_url
                    .as_deref()
                    .or(a.preview_url.as_deref())
                    .or(a.download_url.as_deref())
                    .unwrap_or(a.url.as_str())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or(path.as_str());
        // Prefer the original host path when resolvable (cached); else asset URL.
        let try_src = match resolve(path, cwd.as_deref()) {
            Some(_) => path.as_str(),
            None => src,
        };
        row = row.child(tile(
            try_src,
            cwd.as_deref(),
            asset.map(|a| a.mime.as_str()),
            asset.and_then(|a| a.width),
            asset.and_then(|a| a.height),
            thumb_w,
            thumb_h,
        ));
    }
    row
}

/// `image_assets` embedded from markdown (assistant / user).
pub fn asset_row(ctx: &PartCtx<'_>, assets: &[MessageImageAsset]) -> Div {
    let cwd = session_cwd(ctx);
    let max_w = ctx.width.min(MAX_MEDIA_W);
    let many = assets.len() > 1;
    let thumb_w = if many { STRIP_THUMB } else { max_w };
    let thumb_h = if many { STRIP_THUMB } else { MAX_MEDIA_H };
    let mut row = div()
        .class("mq-attach-row")
        .class(if many {
            "mq-attach-row--strip"
        } else {
            "mq-attach-row--hero"
        })
        .w_full()
        .flex_row()
        .flex_wrap();
    for asset in assets {
        if asset.unavailable_reason.is_some() {
            continue;
        }
        if asset.src.is_empty() && asset.url.is_empty() {
            continue;
        }
        let candidates = [
            asset.src.as_str(),
            asset.url.as_str(),
            asset.preview_url.as_deref().unwrap_or(""),
            asset.thumbnail_url.as_deref().unwrap_or(""),
            asset.download_url.as_deref().unwrap_or(""),
        ];
        let src = candidates
            .into_iter()
            .find(|s| !s.is_empty() && resolve(s, cwd.as_deref()).is_some())
            .or_else(|| candidates.into_iter().find(|s| !s.is_empty()))
            .unwrap_or("");
        if src.is_empty() {
            continue;
        }
        row = row.child(tile(
            src,
            cwd.as_deref(),
            Some(asset.mime.as_str()),
            asset.width,
            asset.height,
            thumb_w,
            thumb_h,
        ));
    }
    row
}

/// File / image / video entries inside a tool call's `content` array.
pub fn tool_content_media(ctx: &PartCtx<'_>, content: &serde_json::Value) -> Option<Div> {
    let items = content.as_array()?;
    let cwd = session_cwd(ctx);
    let max_w = (ctx.width - 24.0).min(MAX_MEDIA_W);
    let mut row = div()
        .class("mq-attach-row")
        .class("mq-attach-row--tool")
        .w_full()
        .flex_row()
        .flex_wrap();
    let mut any = false;
    for item in items {
        let typ = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let uri = item
            .get("uri")
            .or_else(|| item.get("url"))
            .or_else(|| item.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mime = item.get("mime").and_then(|v| v.as_str());
        if uri.is_empty() {
            continue;
        }
        let kind = kind_for(uri, mime);
        if matches!(typ, "file" | "image" | "video")
            || matches!(kind, MediaKind::Image | MediaKind::Video)
        {
            any = true;
            row = row.child(tile(
                uri,
                cwd.as_deref(),
                mime,
                None,
                None,
                max_w,
                MAX_MEDIA_H,
            ));
        }
    }
    any.then_some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_kind_from_mime_and_extension() {
        assert_eq!(kind_for("x", Some("image/png")), MediaKind::Image);
        assert_eq!(kind_for("clip.mp4", None), MediaKind::Video);
        assert_eq!(
            kind_for("notes.pdf", Some("application/pdf")),
            MediaKind::File
        );
    }

    #[test]
    fn fit_keeps_aspect_inside_the_box() {
        let (w, h) = fit_size(Some(2000), Some(1000), 400.0, 280.0);
        assert!((w - 400.0).abs() < 1.0);
        assert!((h - 200.0).abs() < 1.0);
    }
}
