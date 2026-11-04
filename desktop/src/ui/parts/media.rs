//! Inline images and video in the timeline: large previews, theme hairlines,
//! open-in-OS for video. Paths resolve through
//! [`muqun_gateway::agents::message_images::resolve_host_media`].

use std::path::{Path, PathBuf};

use blinc_layout::prelude::*;
use muqun_gateway::agents::message_images::{peek_image_dimensions, resolve_host_media};
use muqun_gateway::agents::MessageImageAsset;

use super::{meta, mono, PartCtx};
use crate::platform::shell::open_path;

/// Cap for a single inline image / video frame in the column (message assets).
pub const MAX_MEDIA_W: f32 = 480.0;
pub const MAX_MEDIA_H: f32 = 320.0;
/// Tool / `read` path previews: App-like caps (`content-image-layout` ~280).
const TOOL_MEDIA_W: f32 = 280.0;
const TOOL_MEDIA_H: f32 = 240.0;
/// Unknown tool image size before decode (App's 160×110).
const TOOL_UNKNOWN_W: f32 = 160.0;
const TOOL_UNKNOWN_H: f32 = 110.0;
/// Unknown message attachment before header probe (must stay narrow so a
/// user bubble does not stretch to the column while waiting on dims).
const ATTACH_UNKNOWN_W: f32 = 200.0;
const ATTACH_UNKNOWN_H: f32 = 150.0;
/// Compact strip thumb when several images share a row.
const STRIP_THUMB: f32 = 112.0;
/// Gap between strip thumbs (matches `.mq-attach-row` gap).
const ATTACH_GAP: f32 = 8.0;

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

fn fit_size_with_unknown(
    w: Option<u32>,
    h: Option<u32>,
    max_w: f32,
    max_h: f32,
    unknown_w: f32,
    unknown_h: f32,
) -> (f32, f32) {
    let (ow, oh) = match (w, h) {
        (Some(w), Some(h)) if w > 0 && h > 0 => (w as f32, h as f32),
        _ => (unknown_w.min(max_w), unknown_h.min(max_h)),
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

type DimsCache = std::collections::HashMap<PathBuf, Option<(u32, u32)>>;

/// Header probe for layout; cached across row rebuilds.
fn dims_of(path: &Path) -> Option<(u32, u32)> {
    use std::sync::{Mutex, OnceLock};
    const CAP: usize = 256;
    static CACHE: OnceLock<Mutex<DimsCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(DimsCache::new()));
    if let Ok(guard) = cache.lock() {
        if let Some(hit) = guard.get(path) {
            return *hit;
        }
    }
    let dims = peek_image_dimensions(path);
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= CAP {
            guard.clear();
        }
        guard.insert(path.to_path_buf(), dims);
    }
    dims
}

fn resolve_dims(
    src: &str,
    cwd: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
) -> (Option<u32>, Option<u32>) {
    if matches!((width, height), (Some(w), Some(h)) if w > 0 && h > 0) {
        return (width, height);
    }
    resolve(src, cwd)
        .and_then(|path| dims_of(&path))
        .map(|(w, h)| (Some(w), Some(h)))
        .unwrap_or((width, height))
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
    tile_sized(
        src,
        cwd,
        mime,
        width,
        height,
        max_w,
        max_h,
        ATTACH_UNKNOWN_W,
        ATTACH_UNKNOWN_H,
    )
}

/// Fitted width of user / assistant attachments so the bubble can hug them.
pub fn measure_attachments_width(
    paths: &[String],
    assets: Option<&[MessageImageAsset]>,
    cwd: Option<&str>,
    max_w: f32,
) -> f32 {
    if paths.is_empty() {
        return 0.0;
    }
    if paths.len() > 1 {
        let n = paths.len() as f32;
        return (n * STRIP_THUMB + (n - 1.0) * ATTACH_GAP).min(max_w);
    }
    let path = &paths[0];
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
    let try_src = match resolve(path, cwd) {
        Some(_) => path.as_str(),
        None => src,
    };
    let (w, h) = resolve_dims(
        try_src,
        cwd,
        asset.and_then(|a| a.width),
        asset.and_then(|a| a.height),
    );
    fit_size_with_unknown(w, h, max_w, MAX_MEDIA_H, ATTACH_UNKNOWN_W, ATTACH_UNKNOWN_H).0
}

/// Fitted width of markdown `image_assets` for bubble sizing.
pub fn measure_assets_width(assets: &[MessageImageAsset], cwd: Option<&str>, max_w: f32) -> f32 {
    let usable: Vec<_> = assets
        .iter()
        .filter(|a| a.unavailable_reason.is_none())
        .filter(|a| !a.src.is_empty() || !a.url.is_empty())
        .collect();
    if usable.is_empty() {
        return 0.0;
    }
    if usable.len() > 1 {
        let n = usable.len() as f32;
        return (n * STRIP_THUMB + (n - 1.0) * ATTACH_GAP).min(max_w);
    }
    let asset = usable[0];
    let candidates = [
        asset.src.as_str(),
        asset.url.as_str(),
        asset.preview_url.as_deref().unwrap_or(""),
        asset.thumbnail_url.as_deref().unwrap_or(""),
        asset.download_url.as_deref().unwrap_or(""),
    ];
    let src = candidates
        .into_iter()
        .find(|s| !s.is_empty() && resolve(s, cwd).is_some())
        .or_else(|| candidates.into_iter().find(|s| !s.is_empty()))
        .unwrap_or("");
    if src.is_empty() {
        return ATTACH_UNKNOWN_W.min(max_w);
    }
    let (w, h) = resolve_dims(src, cwd, asset.width, asset.height);
    fit_size_with_unknown(w, h, max_w, MAX_MEDIA_H, ATTACH_UNKNOWN_W, ATTACH_UNKNOWN_H).0
}

/// Tool / `read` preview: tighter caps and App-like unknown size.
fn tool_tile(
    src: &str,
    cwd: Option<&str>,
    mime: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    max_w: f32,
    max_h: f32,
) -> Div {
    tile_sized(
        src,
        cwd,
        mime,
        width,
        height,
        max_w,
        max_h,
        TOOL_UNKNOWN_W,
        TOOL_UNKNOWN_H,
    )
}

fn tile_sized(
    src: &str,
    cwd: Option<&str>,
    mime: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    max_w: f32,
    max_h: f32,
    unknown_w: f32,
    unknown_h: f32,
) -> Div {
    let kind = kind_for(src, mime);
    let (width, height) = resolve_dims(src, cwd, width, height);
    match resolve(src, cwd) {
        Some(path) if kind == MediaKind::Image => {
            let (w, h) =
                fit_size_with_unknown(width, height, max_w, max_h, unknown_w, unknown_h);
            image_frame(path, w, h, true)
        }
        Some(path) if kind == MediaKind::Video => {
            let (w, h) = fit_size_with_unknown(
                width.or(Some(16)),
                height.or(Some(9)),
                max_w,
                max_h,
                unknown_w,
                unknown_h,
            );
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
///
/// `max_w` caps a single hero preview (caller passes the bubble / column
/// budget so the frame never forces the card wider than content).
pub fn attachment_row(
    ctx: &PartCtx<'_>,
    paths: &[String],
    assets: Option<&[MessageImageAsset]>,
    max_w: f32,
) -> Div {
    let cwd = session_cwd(ctx);
    let max_w = max_w.min(MAX_MEDIA_W).max(48.0);
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
        .flex_row()
        .flex_wrap();
    // Strips may wrap; hero hugs the tile so the bubble stays tight.
    if many {
        row = row.w_full();
    }
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
pub fn asset_row(ctx: &PartCtx<'_>, assets: &[MessageImageAsset], max_w: f32) -> Div {
    let cwd = session_cwd(ctx);
    let max_w = max_w.min(MAX_MEDIA_W).max(48.0);
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
        .flex_row()
        .flex_wrap();
    if many {
        row = row.w_full();
    }
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

/// Inline preview for a host path the tool named (e.g. `read` of a `.png`).
pub fn path_media(ctx: &PartCtx<'_>, path: &str) -> Option<Div> {
    if path.is_empty() {
        return None;
    }
    if !matches!(
        kind_for(path, None),
        MediaKind::Image | MediaKind::Video
    ) {
        return None;
    }
    let cwd = session_cwd(ctx);
    let max_w = (ctx.width - 24.0).min(TOOL_MEDIA_W);
    Some(
        div()
            .class("mq-attach-row")
            .class("mq-attach-row--tool")
            .w_full()
            .flex_row()
            .flex_wrap()
            .child(tool_tile(
                path,
                cwd.as_deref(),
                None,
                None,
                None,
                max_w,
                TOOL_MEDIA_H,
            )),
    )
}

/// File / image / video entries inside a tool call's `content` array.
pub fn tool_content_media(ctx: &PartCtx<'_>, content: &serde_json::Value) -> Option<Div> {
    let items = content.as_array()?;
    let cwd = session_cwd(ctx);
    let max_w = (ctx.width - 24.0).min(TOOL_MEDIA_W);
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
            row = row.child(tool_tile(
                uri,
                cwd.as_deref(),
                mime,
                None,
                None,
                max_w,
                TOOL_MEDIA_H,
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
        let (w, h) = fit_size_with_unknown(
            Some(2000),
            Some(1000),
            400.0,
            280.0,
            ATTACH_UNKNOWN_W,
            ATTACH_UNKNOWN_H,
        );
        assert!((w - 400.0).abs() < 1.0);
        assert!((h - 200.0).abs() < 1.0);
    }

    #[test]
    fn tool_unknown_size_stays_compact() {
        let (w, h) = fit_size_with_unknown(
            None,
            None,
            TOOL_MEDIA_W,
            TOOL_MEDIA_H,
            TOOL_UNKNOWN_W,
            TOOL_UNKNOWN_H,
        );
        assert!((w - TOOL_UNKNOWN_W).abs() < 1.0, "{w}");
        assert!((h - TOOL_UNKNOWN_H).abs() < 1.0, "{h}");
    }

    #[test]
    fn attach_unknown_stays_narrower_than_column() {
        let (w, h) = fit_size_with_unknown(
            None,
            None,
            MAX_MEDIA_W,
            MAX_MEDIA_H,
            ATTACH_UNKNOWN_W,
            ATTACH_UNKNOWN_H,
        );
        assert!((w - ATTACH_UNKNOWN_W).abs() < 1.0, "{w}");
        assert!((h - ATTACH_UNKNOWN_H).abs() < 1.0, "{h}");
        assert!(w < MAX_MEDIA_W);
    }

    #[test]
    fn portrait_attach_hugs_fitted_width() {
        // Tall settings shot capped by height → frame narrower than max_w.
        let (w, h) = fit_size_with_unknown(
            Some(400),
            Some(900),
            MAX_MEDIA_W,
            MAX_MEDIA_H,
            ATTACH_UNKNOWN_W,
            ATTACH_UNKNOWN_H,
        );
        assert!((h - MAX_MEDIA_H).abs() < 1.0, "{h}");
        assert!(w < MAX_MEDIA_W * 0.5, "{w}");
    }
}
