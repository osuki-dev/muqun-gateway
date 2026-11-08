//! Pictures in the timeline: where a message's or a tool's images are on
//! disk (App `MessageAttachments` / `ToolFiles`), the thumbnails, and the
//! full-size zoom a click opens (App `image-preview-modal`).

use std::path::{Path, PathBuf};

use muqun_gateway::agents::message_images::resolve_host_media;
use muqun_gateway::agents::{MessageImageAsset, TimelineItem, ToolCall};
use rune::{col, el, icon, row, text, Cursor, El, SurfaceFit};
use rune_kit::{Kit, Spinner};

use super::{glyphs, Tcx};
use crate::ui::UiState;

const MAX_W: f32 = 480.0;
const THUMB_H: f32 = 240.0;
const MAX_IMAGES: usize = 8;

pub fn image_ext(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff")
    )
}

fn session_directory<'a>(s: &'a UiState, asid: &str) -> Option<&'a str> {
    s.session_by_id(asid).and_then(|i| i.directory.as_deref())
}

fn push_resolved(out: &mut Vec<PathBuf>, src: &str, directory: Option<&str>) {
    let Some(path) = resolve_host_media(src, directory) else {
        return;
    };
    if !image_ext(&path) && !path.is_file() {
        return;
    }
    if path.is_file() && !out.iter().any(|p| p == &path) {
        out.push(path);
    }
}

/// A gateway URL's picture: the local file it stands for, else the copy
/// fetched into the media cache.
fn push_hosted(out: &mut Vec<PathBuf>, src: &str, directory: Option<&str>) {
    let before = out.len();
    push_resolved(out, src, directory);
    if out.len() == before {
        if let Some(path) = crate::model::media::cached(src) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
}

/// App `messageImageForSource`: exact `src` match, image mime, not unavailable.
fn message_image_for_source<'a>(
    assets: Option<&'a [MessageImageAsset]>,
    src: &str,
) -> Option<&'a MessageImageAsset> {
    assets?.iter().find(|asset| {
        asset.src == src
            && asset.unavailable_reason.is_none()
            && asset.mime.starts_with("image/")
            && asset.mime[6..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-'))
    })
}

fn looks_like_image_uri(uri: &str) -> bool {
    uri.starts_with("data:image/")
        || matches!(
            Path::new(uri.split('?').next().unwrap_or(uri))
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("png" | "jpg" | "jpeg" | "gif" | "webp")
        )
}

fn push_asset(out: &mut Vec<PathBuf>, asset: &MessageImageAsset, directory: Option<&str>) {
    let before = out.len();
    for candidate in [
        asset.thumbnail_url.as_deref(),
        asset.preview_url.as_deref(),
        (!asset.url.is_empty()).then_some(asset.url.as_str()),
        (!asset.src.is_empty()).then_some(asset.src.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        push_hosted(out, candidate, directory);
        if out.len() > before {
            break;
        }
    }
}

/// App `MessageAttachments`: walk `attachments`, resolve each through
/// `image_assets`, else treat a clear image URI as a local file; with no
/// attachments, the assets a markdown body embeds.
pub fn item_image_paths(s: &UiState, asid: &str, item: &TimelineItem) -> Vec<PathBuf> {
    let directory = session_directory(s, asid);
    let mut paths = Vec::new();
    let assets = item.image_assets.as_deref();
    for src in item.attachments.iter().flatten() {
        if let Some(asset) = message_image_for_source(assets, src) {
            push_asset(&mut paths, asset, directory);
        } else if looks_like_image_uri(src) || src.starts_with("/api/") {
            push_hosted(&mut paths, src, directory);
        }
        if paths.len() >= MAX_IMAGES {
            return paths;
        }
    }
    if paths.is_empty() {
        for asset in assets.into_iter().flatten() {
            if asset.unavailable_reason.is_some() || !asset.mime.starts_with("image/") {
                continue;
            }
            push_asset(&mut paths, asset, directory);
            if paths.len() >= MAX_IMAGES {
                break;
            }
        }
    }
    paths
}

/// Pictures a message carries that are not drawable yet (still being
/// fetched from the gateway): each holds a placeholder.
pub fn pending_images(item: &TimelineItem, shown: usize) -> usize {
    let carried = item
        .image_assets
        .iter()
        .flatten()
        .filter(|a| a.unavailable_reason.is_none() && a.mime.starts_with("image/"))
        .count();
    carried.min(MAX_IMAGES).saturating_sub(shown)
}

/// Whether a message carries pictures at all (shown or on their way).
pub fn carries_images(item: &TimelineItem) -> bool {
    item.attachments.as_ref().is_some_and(|a| !a.is_empty())
        || item
            .image_assets
            .iter()
            .flatten()
            .any(|a| a.unavailable_reason.is_none() && a.mime.starts_with("image/"))
}

/// The images a tool returned (`content[]` file / image items, ACP's nested
/// `content`, inline pictures the core kept as files), and the picture a
/// read-like tool opened.
pub fn tool_image_paths(s: &UiState, asid: &str, call: &ToolCall) -> Vec<PathBuf> {
    let directory = session_directory(s, asid);
    let mut paths = Vec::new();
    for src in crate::model::media::tool_image_refs(call) {
        push_hosted(&mut paths, &src, directory);
        if paths.len() >= MAX_IMAGES {
            break;
        }
    }
    paths
}

/// A picture's size from its header, remembered: rows are laid out before
/// the picture decodes, and a row that changes height later jumps.
fn dimensions(path: &Path) -> Option<[u32; 2]> {
    thread_local! {
        static SIZES: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<[u32; 2]>>> =
            Default::default();
    }
    SIZES.with(|sizes| {
        *sizes
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_insert_with(|| image::image_dimensions(path).ok().map(|(w, h)| [w, h]))
    })
}

/// One picture at `height`, as wide as its shape asks (within reason): a
/// button that opens it full size.
fn thumbnail(k: &Kit, s: &UiState, path: &Path, key: String, height: f32) -> El {
    let Some(images) = s.images.as_ref().filter(|_| path.is_file()) else {
        return placeholder(k, key, height);
    };
    // `ImageCache::file` keeps a strong handle via Pixels::keep while drawn.
    let image = images.file(path, THUMB_H, 2.0);
    // The picture's own shape: a wide one gets shorter, never cropped.
    let (width, height) = match image.size().or_else(|| dimensions(path)) {
        Some([w, h]) if w > 0 && h > 0 => {
            let aspect = w as f32 / h as f32;
            let width = (height * aspect).clamp(64.0, MAX_W);
            (width, (width / aspect).clamp(40.0, height))
        }
        _ => (height * 4.0 / 3.0, height),
    };
    let open = path.to_path_buf();
    let label = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    k.interactive(
        el().key(key)
            .pixels_fit(image.pixels(), SurfaceFit::Fill)
            .w(width)
            .shrink()
            .h(height)
            .radius(k.radius.md)
            .clip_rounded(k.radius.md)
            .border(1.0, k.c.border_weak),
    )
    .cursor(Cursor::Pointer)
    .label(label)
    .on_press(move |s: &mut UiState| s.zoom = Some(open.clone()))
}

/// Where a picture on its way will be.
fn placeholder(k: &Kit, key: String, height: f32) -> El {
    col()
        .key(key)
        .w(height * 4.0 / 3.0)
        .h(height)
        .center()
        .radius(k.radius.md)
        .fill(k.c.layer_02)
        .child(
            icon(&rune_kit::glyphs::FILE_IMAGE, 18.0)
                .color(k.c.icon_faint)
                .a11y_hidden(),
        )
}

/// A row of pictures (wrapping), each opening full size, then `pending`
/// placeholders for those still on their way.
pub fn gallery(cx: &Tcx, id: &str, paths: &[PathBuf]) -> Option<El> {
    gallery_sized(cx, id, paths, 0, THUMB_H)
}

pub fn gallery_sized(
    cx: &Tcx,
    id: &str,
    paths: &[PathBuf],
    pending: usize,
    height: f32,
) -> Option<El> {
    cx.s.images.as_ref()?;
    if paths.is_empty() && pending == 0 {
        return None;
    }
    let mut gallery = row()
        .key(format!("imgs-{id}"))
        .gap(8.0)
        .flex_wrap()
        .w_full();
    for (i, path) in paths.iter().enumerate() {
        gallery = gallery.child(thumbnail(cx.k, cx.s, path, format!("img-{id}-{i}"), height));
    }
    for i in 0..pending {
        gallery = gallery.child(placeholder(cx.k, format!("img-{id}-p{i}"), height));
    }
    Some(gallery)
}

/// The open picture over the whole window: fitted, on a scrim; a click
/// outside it, the close button or Escape closes it.
pub fn zoom_overlay(s: &UiState, k: &Kit) -> Option<El> {
    let path = s.zoom.as_ref()?;
    let images = s.images.as_ref()?;
    let language = s.cfg.general.language;
    let close = |s: &mut UiState| s.zoom = None;
    // Raster files decode whole whatever the size asked for, so the
    // thumbnail's handle is already the full picture.
    let image = images.file(path, THUMB_H, 2.0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let picture = if image.is_ready() {
        el().key("zoom-picture")
            .pixels_fit(image.pixels(), SurfaceFit::Fit)
            .w_full()
            .grow(1.0)
            .min_h(0.0)
    } else {
        col()
            .key("zoom-picture")
            .w_full()
            .grow(1.0)
            .center()
            .child(Spinner::new().build(k))
    };
    let bar = row()
        .key("zoom-bar")
        .w_full()
        .gap(12.0)
        .center_y()
        .child(
            text(name)
                .font(k.ty.ui_medium)
                .color(k.c.white)
                .single_line()
                .truncate_middle()
                .shrink()
                .grow(1.0),
        )
        .child(
            k.interactive(
                row()
                    .key("zoom-close")
                    .size(32.0, 32.0)
                    .center()
                    .radius(16.0)
                    .child(icon(&glyphs::CLOSE, 16.0).color(k.c.white)),
            )
            .cursor(Cursor::Pointer)
            .label(crate::ui::copy::tr_ref(language, "Close", "关闭"))
            .on_press(close),
        );
    let mut stage = col()
        .key("timeline-zoom")
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .px(40.0)
        .py(28.0)
        .gap(14.0)
        .fill(k.alpha(k.c.black, 0.88))
        .on_press(close)
        .modal(true, close)
        .child(bar)
        .child(picture);
    stage = if s.cfg.appearance.reduced_motion {
        stage.transition(rune::Transition::opacity())
    } else {
        stage.transition(rune::Transition::scale(0.97))
    };
    Some(stage)
}
