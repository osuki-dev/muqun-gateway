//! Images an agent's markdown embeds by host path.
//!
//! An agent that renders a chart and says `![flow](./out/flow.png)` -- or
//! `file:///home/me/work/out/flow.png`, or the bare absolute path -- has
//! written a reference only its own machine can follow. The phone is handed
//! that string and has nothing to load it from, so the image is a blank.
//!
//! The text is left exactly as the agent wrote it: copy, export and "copy as
//! markdown" keep carrying the original paths. What changes is that each text
//! part leaving the gateway carries `image_assets`, a map from every image
//! source in it that resolves to an image file inside the session's directory
//! to the asset URL that serves that file. A client swaps the source for the
//! URL at render time; a source missing from the map -- a web URL, a data URI,
//! a path outside the directory, something that is not an image -- is left
//! alone, and an older gateway that sends no map at all leaves every image
//! exactly as before.
//!
//! The map is attached on the way out (snapshot, timeline, event replay, the
//! SSE stream and `/api/ws`), never stored, so the mirror and its event log
//! keep the agent's own parts. Every image listed is remembered here with the
//! directory that fenced it, which is what lets `GET /api/assets/{id}/content`
//! serve it: the id is the same path-derived `asset_id` the workspace index
//! uses, and the read is checked against that directory again at serve time.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::agents::domain::{AgentDomainEvent, AgentPart, MessageImageAsset, TimelineItem};
use crate::{
    api_error, asset_id, asset_json, content_envelope, read_asset_head, require_device,
    sniff_asset_type, ApiResult, AppState, AssetEntry, AssetKind,
};

/// How many served images are remembered. Old entries fall off; the next read
/// of the timeline that names them puts them back.
const MAX_SERVED_IMAGES: usize = 4096;
/// A part that embeds more images than this is not worth a filesystem walk for
/// each one on every streamed update.
const MAX_IMAGES_PER_PART: usize = 32;
/// How many session directories are remembered.
const MAX_SESSION_ROOTS: usize = 1024;

/// One image the gateway has told a client it may fetch, and the directory
/// that has to contain it when it is fetched.
#[derive(Debug, Clone)]
pub(crate) struct ServedImage {
    pub(crate) path: PathBuf,
    pub(crate) root: PathBuf,
}

#[derive(Default)]
struct Inner {
    /// Session id to its canonical directory.
    roots: HashMap<String, PathBuf>,
    root_order: VecDeque<String>,
    served: HashMap<String, ServedImage>,
    served_order: VecDeque<String>,
}

#[derive(Default)]
pub(crate) struct MessageImages {
    inner: Mutex<Inner>,
}

impl MessageImages {
    /// The image behind an id this gateway handed out, if it still remembers it.
    pub(crate) fn served(&self, id: &str) -> Option<ServedImage> {
        self.inner.lock().ok()?.served.get(id).cloned()
    }

    fn root_for(&self, asid: &str) -> Option<PathBuf> {
        self.inner.lock().ok()?.roots.get(asid).cloned()
    }

    /// Note where a session works, from anything that says so.
    pub(crate) fn remember_root(&self, asid: &str, directory: Option<&str>) -> Option<PathBuf> {
        let root = image_root(directory?)?;
        let mut inner = self.inner.lock().ok()?;
        if inner.roots.insert(asid.to_owned(), root.clone()).is_none() {
            inner.root_order.push_back(asid.to_owned());
            while inner.root_order.len() > MAX_SESSION_ROOTS {
                if let Some(old) = inner.root_order.pop_front() {
                    inner.roots.remove(&old);
                }
            }
        }
        Some(root)
    }

    fn remember_served(&self, id: &str, image: ServedImage) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.served.insert(id.to_owned(), image).is_none() {
            inner.served_order.push_back(id.to_owned());
            while inner.served_order.len() > MAX_SERVED_IMAGES {
                if let Some(old) = inner.served_order.pop_front() {
                    inner.served.remove(&old);
                }
            }
        }
    }

    /// Fill in `image_assets` on every text part that embeds an image this
    /// root can serve. Parts with nothing servable are left without the field.
    pub(crate) fn attach(&self, root: &Path, items: &mut [TimelineItem]) {
        for item in items {
            let AgentPart::Text { text } = &item.part else {
                continue;
            };
            let assets = self.resolve_text(root, text);
            item.image_assets = (!assets.is_empty()).then_some(assets);
        }
    }

    fn resolve_text(&self, root: &Path, text: &str) -> Vec<MessageImageAsset> {
        let mut assets = Vec::new();
        for src in image_sources(text).into_iter().take(MAX_IMAGES_PER_PART) {
            let Some(path) = resolve_image_path(&src, root) else {
                continue;
            };
            let head = read_asset_head(&path);
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = sniff_asset_type(&head, &name);
            if kind.kind != AssetKind::Image {
                continue;
            }
            let id = asset_id(&path);
            let (width, height) = image_dimensions(&head).unzip();
            self.remember_served(
                &id,
                ServedImage {
                    path,
                    root: root.to_path_buf(),
                },
            );
            assets.push(MessageImageAsset {
                src,
                url: format!("/api/assets/{id}/content"),
                asset_id: id,
                mime: kind.mime.to_owned(),
                width,
                height,
            });
        }
        assets
    }
}

/// True when a text part could embed an image at all -- the cheap test that
/// keeps every streamed delta of ordinary prose off the filesystem.
fn may_embed_image(item: &TimelineItem) -> bool {
    matches!(&item.part, AgentPart::Text { text }
        if text.contains("![") || text.to_ascii_lowercase().contains("<img"))
}

/// A session's directory as an image fence: it has to exist, be a directory,
/// and not be the filesystem root.
pub(crate) fn image_root(directory: &str) -> Option<PathBuf> {
    let root = std::fs::canonicalize(directory.trim()).ok()?;
    (root.is_dir() && root.parent().is_some()).then_some(root)
}

/// The directory a session's images are fenced to: remembered, else asked of
/// the mirror, else of the agent that owns the session.
async fn session_root(state: &AppState, asid: &str) -> Option<PathBuf> {
    if let Some(root) = state.message_images.root_for(asid) {
        return Some(root);
    }
    let manager = state.agent_runtime.manager_for_session(asid).await?;
    let directory = match manager.mirror().session_directory(asid).await {
        Some(directory) => directory,
        None => manager.sessions().get_session(asid).await.ok()?.directory?,
    };
    state
        .message_images
        .remember_root(asid, Some(directory.as_str()))
}

#[derive(serde::Deserialize)]
pub(crate) struct AudioAssetQuery {
    pub(crate) uri: String,
}

/// Resolve tool audio against the agent-owned directory, never a client root.
/// The content endpoint authenticates, bounds and fences the subsequent read.
pub(crate) async fn audio_asset(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(asid): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<AudioAssetQuery>,
    headers: axum::http::HeaderMap,
) -> ApiResult<axum::Json<serde_json::Value>> {
    require_device(&state, &headers)?;
    let not_found = || {
        api_error(
            axum::http::StatusCode::NOT_FOUND,
            "audio_not_found",
            "audio output is unavailable inside this session",
        )
    };
    if query.uri.len() > 8192 {
        return Err(not_found());
    }
    let root = session_root(&state, &asid).await.ok_or_else(not_found)?;
    let owned_root = root.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        let path = resolve_image_path(&query.uri, &owned_root)?;
        let name = path.file_name()?.to_string_lossy().into_owned();
        let kind = sniff_asset_type(&read_asset_head(&path), &name);
        if kind.kind != AssetKind::Audio {
            return None;
        }
        let metadata = std::fs::metadata(&path).ok()?;
        Some((path, name, kind, metadata.len()))
    })
    .await
    .unwrap_or_default();
    let (path, name, kind, size) = resolved.ok_or_else(not_found)?;
    let id = asset_id(&path);
    state.message_images.remember_served(
        &id,
        ServedImage {
            path: path.clone(),
            root: root.clone(),
        },
    );
    let entry = AssetEntry {
        id,
        path,
        name,
        size,
        modified_unix_ms: 0,
        root,
        session_id: String::new(),
        workspace_id: None,
        tab_id: None,
        pane_id: None,
    };
    Ok(axum::Json(content_envelope(
        serde_json::json!({ "asset": asset_json(&entry, kind) }),
    )))
}

/// Attach `image_assets` to items of one session. `directory` is used when the
/// caller already holds the session's info.
pub(crate) async fn attach_to_items(
    state: &AppState,
    asid: &str,
    directory: Option<&str>,
    items: &mut [TimelineItem],
) {
    if !items.iter().any(may_embed_image) {
        return;
    }
    let root = match directory {
        Some(directory) => state.message_images.remember_root(asid, Some(directory)),
        None => None,
    };
    let root = match root {
        Some(root) => root,
        None => match session_root(state, asid).await {
            Some(root) => root,
            None => return,
        },
    };
    state.message_images.attach(&root, items);
}

/// The same, for an event on its way to a stream or a replay.
pub(crate) async fn attach_to_event(state: &AppState, event: &mut AgentDomainEvent) {
    match event {
        AgentDomainEvent::SessionUpdated { asid, info, .. } => {
            state
                .message_images
                .remember_root(&asid.0, info.directory.as_deref());
        }
        AgentDomainEvent::TimelineUpsert { asid, items, .. } => {
            attach_to_items(state, &asid.0, None, items).await;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Finding image sources in markdown.

/// Every image source a markdown text embeds, in order and without repeats:
/// the destination of `![alt](dest "title")` (without its `<...>` wrapper) and
/// the `src` of an HTML `<img>`. Fenced code blocks and inline code spans are
/// skipped -- an image written there is shown as code, not loaded.
pub(crate) fn image_sources(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    for line in markdown.lines() {
        if let Some((marker, count, rest)) = fence_marker(line) {
            match fence {
                None => {
                    fence = Some((marker, count));
                    continue;
                }
                Some((open, open_count))
                    if open == marker && count >= open_count && rest.trim().is_empty() =>
                {
                    fence = None;
                    continue;
                }
                Some(_) => continue,
            }
        }
        if fence.is_some() {
            continue;
        }
        for segment in prose_segments(line) {
            markdown_image_sources(segment, &mut out);
            html_image_sources(segment, &mut out);
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|src| seen.insert(src.clone()));
    out
}

/// A fence line: up to three spaces, then three or more backticks or tildes.
fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let count = rest.bytes().take_while(|byte| *byte == marker).count();
    (count >= 3).then(|| (marker, count, &rest[count..]))
}

/// The parts of one line outside inline code spans. A backtick run with no
/// closing run of the same length is literal text, as CommonMark has it.
fn prose_segments(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|byte| **byte == b'`').count();
        let after = i + run;
        let mut j = after;
        let mut close = None;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let other = bytes[j..].iter().take_while(|byte| **byte == b'`').count();
                if other == run {
                    close = Some(j + other);
                    break;
                }
                j += other;
            } else {
                j += 1;
            }
        }
        match close {
            Some(end) => {
                segments.push(&line[start..i]);
                start = end;
                i = end;
            }
            None => i = after,
        }
    }
    segments.push(&line[start..]);
    segments
}

fn markdown_image_sources(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(found) = text[i..].find("![") {
        let open = i + found;
        i = open + 2;
        if open > 0 && bytes[open - 1] == b'\\' {
            continue;
        }
        // The alt text, with nested brackets and escapes.
        let mut depth = 1;
        let mut j = open + 2;
        while j < bytes.len() && depth > 0 {
            match bytes[j] {
                b'\\' => j += 1,
                b'[' => depth += 1,
                b']' => depth -= 1,
                _ => {}
            }
            j += 1;
        }
        if depth != 0 || bytes.get(j) != Some(&b'(') {
            continue;
        }
        j += 1;
        while bytes
            .get(j)
            .is_some_and(|byte| *byte == b' ' || *byte == b'\t')
        {
            j += 1;
        }
        let (start, end) = if bytes.get(j) == Some(&b'<') {
            let Some(close) = text[j + 1..].find('>') else {
                continue;
            };
            (j + 1, j + 1 + close)
        } else {
            let start = j;
            let mut parens = 0usize;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => j += 1,
                    b'(' => parens += 1,
                    b')' if parens == 0 => break,
                    b')' => parens -= 1,
                    b' ' | b'\t' => break,
                    _ => {}
                }
                j += 1;
            }
            (start, j.min(bytes.len()))
        };
        let src = &text[start..end];
        if !src.is_empty() {
            out.push(src.to_owned());
        }
        i = end.max(i);
    }
}

fn html_image_sources(text: &str, out: &mut Vec<String>) {
    let lower = text.to_ascii_lowercase();
    let mut i = 0;
    while let Some(found) = lower[i..].find("<img") {
        let tag_start = i + found + 4;
        let tag_end = lower[tag_start..]
            .find('>')
            .map_or(lower.len(), |end| tag_start + end);
        i = tag_end;
        let tag = &lower[tag_start..tag_end];
        let mut k = 0;
        while let Some(at) = tag[k..].find("src") {
            let name = k + at;
            k = name + 3;
            let preceded = name == 0
                || tag.as_bytes()[name - 1].is_ascii_whitespace()
                || tag.as_bytes()[name - 1] == b'/';
            if !preceded {
                continue;
            }
            let rest = tag[k..].trim_start();
            let Some(rest) = rest.strip_prefix('=') else {
                continue;
            };
            let value_offset = tag_start + (tag.len() - rest.trim_start().len());
            let original = &text[value_offset..tag_end];
            let src = match original.as_bytes().first() {
                Some(quote @ (b'"' | b'\'')) => original[1..]
                    .find(*quote as char)
                    .map(|end| &original[1..1 + end]),
                Some(_) => Some(
                    original
                        .split(|c: char| c.is_ascii_whitespace() || c == '/')
                        .next()
                        .unwrap_or(""),
                ),
                None => None,
            };
            if let Some(src) = src.filter(|src| !src.is_empty()) {
                out.push(src.to_owned());
            }
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Resolving one source.

/// The file an image source names, if it is a regular file strictly inside
/// `root` (which must already be canonical). Relative sources are read against
/// `root`; `file://` URLs and absolute paths as they are; `~/` against the
/// gateway account's home. Anything with another scheme is not a host path.
pub(crate) fn resolve_image_path(src: &str, root: &Path) -> Option<PathBuf> {
    let src = src.trim();
    if src.is_empty() || src.starts_with("//") || src.starts_with("/api/") {
        return None;
    }
    let (raw, decode_always) = if let Some(rest) = strip_prefix_ignore_case(src, "file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        (rest.to_owned(), true)
    } else if has_scheme(src) {
        return None;
    } else if let Some(rest) = src.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        (home.join(rest).to_string_lossy().into_owned(), false)
    } else {
        (src.to_owned(), false)
    };
    let candidates = if decode_always {
        vec![percent_decode(&raw)?]
    } else {
        let mut candidates = vec![raw.clone()];
        if raw.contains('%') {
            candidates.extend(percent_decode(&raw));
        }
        candidates
    };
    candidates.into_iter().find_map(|candidate| {
        let canonical = std::fs::canonicalize(root.join(candidate)).ok()?;
        (canonical.is_file() && canonical != root && canonical.starts_with(root))
            .then_some(canonical)
    })
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `scheme:` at the start, as a URL has it. Two characters at least, so a
/// Windows drive letter is not mistaken for one.
fn has_scheme(src: &str) -> bool {
    let Some(colon) = src.find(':') else {
        return false;
    };
    let scheme = &src[..colon];
    scheme.len() >= 2
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

/// `%XX` decoding into UTF-8; `None` for a malformed escape or invalid UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Width and height from the first bytes of a PNG, GIF, WebP or JPEG, when
/// they are there. Best effort: a client lays out without them.
pub(crate) fn image_dimensions(head: &[u8]) -> Option<(u32, u32)> {
    let be32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(head.get(at..at + 4)?.try_into().ok()?))
    };
    let le16 = |at: usize| -> Option<u32> {
        Some(u16::from_le_bytes(head.get(at..at + 2)?.try_into().ok()?) as u32)
    };
    let be16 = |at: usize| -> Option<u32> {
        Some(u16::from_be_bytes(head.get(at..at + 2)?.try_into().ok()?) as u32)
    };
    let le24 = |at: usize| -> Option<u32> {
        let b = head.get(at..at + 3)?;
        Some(b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16)
    };
    if head.starts_with(b"\x89PNG\r\n\x1a\n") && head.get(12..16) == Some(b"IHDR") {
        return Some((be32(16)?, be32(20)?));
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some((le16(6)?, le16(8)?));
    }
    if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return match head.get(12..16)? {
            b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
            b"VP8L" => {
                let b = head.get(21..25)?;
                let bits = u32::from_le_bytes(b.try_into().ok()?);
                Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
            }
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            _ => None,
        };
    }
    if head.starts_with(b"\xff\xd8") {
        let mut i = 2;
        while i + 9 < head.len() {
            if head[i] != 0xff {
                return None;
            }
            let marker = head[i + 1];
            let length = be16(i + 2)? as usize;
            let is_frame = matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
            if is_frame {
                return Some((be16(i + 7)?, be16(i + 5)?));
            }
            i += 2 + length;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_audio_is_authenticated_fenced_and_served_by_opaque_id() {
        use crate::{asset_content, bearer_headers, test_device, test_state};
        use axum::extract::{Path as RoutePath, Query, State};
        use axum::http::{HeaderMap, StatusCode};
        let (scratch, root) = workspace();
        let bytes = b"RIFF\x24\x00\x00\x00WAVEfmt ";
        std::fs::write(root.join("out/sample.wav"), bytes).unwrap();
        std::fs::write(scratch.path().join("outside.wav"), bytes).unwrap();
        let state = test_state("admin", vec![test_device("d1", "token")]);
        state
            .message_images
            .remember_root("agent-audio", root.to_str());
        let query = || {
            Query(AudioAssetQuery {
                uri: "out/sample.wav".into(),
            })
        };
        assert_eq!(
            audio_asset(
                State(state.clone()),
                RoutePath("agent-audio".into()),
                query(),
                HeaderMap::new()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
        let response = audio_asset(
            State(state.clone()),
            RoutePath("agent-audio".into()),
            query(),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        let asset = &response.0["data"]["asset"];
        assert_eq!(asset["kind"], "audio");
        assert_eq!(asset["mime"], "audio/wav");
        let id = asset["id"].as_str().unwrap();
        let content = asset_content(
            State(state.clone()),
            RoutePath(id.to_owned()),
            bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(
            axum::body::to_bytes(content.into_body(), 1024)
                .await
                .unwrap()
                .as_ref(),
            bytes
        );
        for uri in [
            "../outside.wav",
            "https://example.invalid/audio.wav",
            "out/flow.png",
        ] {
            assert_eq!(
                audio_asset(
                    State(state.clone()),
                    RoutePath("agent-audio".into()),
                    Query(AudioAssetQuery { uri: uri.into() }),
                    bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::NOT_FOUND
            );
        }
        #[cfg(unix)]
        {
            std::fs::remove_file(root.join("out/sample.wav")).unwrap();
            std::os::unix::fs::symlink(
                scratch.path().join("outside.wav"),
                root.join("out/sample.wav"),
            )
            .unwrap();
            assert_eq!(
                asset_content(
                    State(state),
                    RoutePath(id.to_owned()),
                    bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::NOT_FOUND
            );
        }
    }

    static SCRATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        bytes.extend(width.to_be_bytes());
        bytes.extend(height.to_be_bytes());
        bytes.extend([8, 6, 0, 0, 0]);
        bytes
    }

    /// A scratch directory holding a `work` root, an image outside it, and the
    /// canonical root. Removed when dropped.
    fn workspace() -> (Scratch, PathBuf) {
        let dir = Scratch(std::env::temp_dir().join(format!(
            "muqun-message-images-{}-{}",
            std::process::id(),
            SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )));
        std::fs::create_dir_all(dir.path()).unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("work");
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::create_dir_all(root.join("图 表")).unwrap();
        std::fs::write(root.join("out/flow.png"), png(640, 480)).unwrap();
        std::fs::write(root.join("图 表/深色 流程.png"), png(10, 20)).unwrap();
        std::fs::write(root.join("notes.md"), "# not an image\n").unwrap();
        std::fs::write(dir.path().join("outside.png"), png(1, 1)).unwrap();
        (dir, root)
    }

    fn item(text: &str) -> TimelineItem {
        TimelineItem {
            id: "m1:t0".into(),
            message_id: "m1".into(),
            role: crate::agents::domain::TimelineRole::Assistant,
            part: AgentPart::Text { text: text.into() },
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        }
    }

    fn attached(root: &Path, text: &str) -> Vec<MessageImageAsset> {
        let images = MessageImages::default();
        let mut items = vec![item(text)];
        images.attach(root, &mut items);
        items.remove(0).image_assets.unwrap_or_default()
    }

    #[test]
    fn finds_markdown_and_html_sources_outside_code() {
        let text = "intro ![a](./x.png) and ![b c](<my file.png> \"title\")\n\
            ![t](p.png 'single') `![no](code.png)` <img alt=x src=\"h.png\"/>\n\
            ```md\n![no](fenced.png)\n```\n\
            [![linked](inner.png)](https://example.com) ![d](dir/(1).png)\n\
            \\![escaped](no.png) ![a](./x.png) <IMG SRC='upper.png'>";
        assert_eq!(
            image_sources(text),
            [
                "./x.png",
                "my file.png",
                "p.png",
                "h.png",
                "inner.png",
                "dir/(1).png",
                "upper.png"
            ]
        );
    }

    #[test]
    fn relative_absolute_and_file_urls_resolve_inside_the_root() {
        let (_dir, root) = workspace();
        let flow = root.join("out/flow.png");
        let absolute = flow.to_string_lossy().into_owned();
        assert_eq!(
            resolve_image_path("out/flow.png", &root),
            Some(flow.clone())
        );
        assert_eq!(
            resolve_image_path("./out/flow.png", &root),
            Some(flow.clone())
        );
        assert_eq!(resolve_image_path(&absolute, &root), Some(flow.clone()));
        assert_eq!(
            resolve_image_path(&format!("file://{absolute}"), &root),
            Some(flow.clone())
        );
        assert_eq!(
            resolve_image_path(&format!("FILE://localhost{absolute}"), &root),
            Some(flow)
        );
    }

    #[test]
    fn spaces_and_cjk_resolve_raw_or_percent_encoded() {
        let (_dir, root) = workspace();
        let cjk = root.join("图 表/深色 流程.png");
        assert_eq!(
            resolve_image_path("图 表/深色 流程.png", &root),
            Some(cjk.clone())
        );
        assert_eq!(
            resolve_image_path(
                "%E5%9B%BE%20%E8%A1%A8/%E6%B7%B1%E8%89%B2%20%E6%B5%81%E7%A8%8B.png",
                &root
            ),
            Some(cjk.clone())
        );
        let url = format!("file://{}", cjk.to_string_lossy().replace(' ', "%20"));
        assert_eq!(resolve_image_path(&url, &root), Some(cjk));
    }

    #[test]
    fn nothing_outside_the_root_or_off_host_resolves() {
        let (dir, root) = workspace();
        let outside = dir.path().join("outside.png");
        assert_eq!(resolve_image_path("../outside.png", &root), None);
        assert_eq!(resolve_image_path("out/../../outside.png", &root), None);
        assert_eq!(resolve_image_path(&outside.to_string_lossy(), &root), None);
        assert_eq!(resolve_image_path("/etc/hostname", &root), None);
        assert_eq!(resolve_image_path("out", &root), None);
        assert_eq!(resolve_image_path(".", &root), None);
        for off_host in [
            "https://example.com/out/flow.png",
            "http://example.com/x.png",
            "data:image/png;base64,iVBORw0KGgo=",
            "//example.com/out/flow.png",
            "/api/assets/as_0123/content",
        ] {
            assert_eq!(resolve_image_path(off_host, &root), None, "{off_host}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape.png")).unwrap();
            assert_eq!(resolve_image_path("escape.png", &root), None);
        }
    }

    #[test]
    fn a_text_part_gets_a_map_of_what_it_can_serve() {
        let (_dir, root) = workspace();
        let flow = root.join("out/flow.png");
        let text = format!(
            "![flow](./out/flow.png \"Flow\")\n![abs](file://{})\n![web](https://x.test/a.png)\n\
             ![md](notes.md) ![gone](missing.png) ![out](../outside.png)\n<img src=\"out/flow.png\">",
            flow.display()
        );
        let assets = attached(&root, &text);
        let id = asset_id(&flow);
        assert_eq!(assets.len(), 3);
        assert_eq!(assets[0].src, "./out/flow.png");
        assert_eq!(assets[1].src, format!("file://{}", flow.display()));
        assert_eq!(assets[2].src, "out/flow.png");
        for asset in &assets {
            assert_eq!(asset.asset_id, id);
            assert_eq!(asset.url, format!("/api/assets/{id}/content"));
            assert_eq!(asset.mime, "image/png");
            assert_eq!((asset.width, asset.height), (Some(640), Some(480)));
        }
    }

    #[test]
    fn served_images_are_remembered_with_their_root_and_text_is_untouched() {
        let (_dir, root) = workspace();
        let images = MessageImages::default();
        let text = "see ![flow](out/flow.png)";
        let mut items = vec![item(text), item("no images here")];
        images.attach(&root, &mut items);
        assert!(matches!(&items[0].part, AgentPart::Text { text: t } if t == text));
        assert!(items[1].image_assets.is_none());
        let id = &items[0].image_assets.as_ref().unwrap()[0].asset_id;
        let served = images.served(id).unwrap();
        assert_eq!(served.path, root.join("out/flow.png"));
        assert_eq!(served.root, root);
        // Serialised as an optional field next to `attachments`.
        let wire = serde_json::to_value(&items[0]).unwrap();
        assert_eq!(wire["image_assets"][0]["src"], "out/flow.png");
        assert!(serde_json::to_value(&items[1])
            .unwrap()
            .get("image_assets")
            .is_none());
    }

    #[test]
    fn already_rewritten_urls_are_left_alone() {
        let (_dir, root) = workspace();
        let id = asset_id(&root.join("out/flow.png"));
        assert!(attached(&root, &format!("![x](/api/assets/{id}/content)")).is_empty());
    }

    #[test]
    fn the_filesystem_root_is_not_a_fence() {
        assert_eq!(image_root("/"), None);
        assert_eq!(image_root("/definitely/not/here"), None);
    }

    #[test]
    fn dimensions_come_from_the_header() {
        assert_eq!(image_dimensions(&png(3, 4)), Some((3, 4)));
        assert_eq!(image_dimensions(b"GIF89a\x05\x00\x07\x00"), Some((5, 7)));
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00,
            0x20, 0x00, 0x40, 0x03, 0, 0, 0, 0,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((64, 32)));
        assert_eq!(image_dimensions(b"not an image"), None);
    }
}
