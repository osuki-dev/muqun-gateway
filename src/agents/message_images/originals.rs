//! Original-preserving attachment admission and authenticated reads. No filename
//! or adapter-name inference: inline bytes are sniffed, host reads are fenced.
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::body::Body;
use axum::extract::{Path as RoutePath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;

use super::{hosted_file, image_dimensions, MessageImageAsset, ServedImage};
use crate::agents::domain::{AgentPart, TimelineItem};
use crate::{api_error, require_device, ApiResult, AppState, AssetKind};

const MAX_PREVIEW_SOURCE_BYTES: u64 =
    crate::agents::attachment_images::MAX_PREVIEW_SOURCE_BYTES as u64;
const RESPONSE_WORK_BYTES: usize = 64 * 1024 * 1024;
static ADMISSION: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

pub(super) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Create only gateway-owned directories; reject a symlink before changing
/// permissions. Files below are written create-new, never replacing originals.
pub(super) fn private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    if !std::fs::symlink_metadata(path)?.is_dir() {
        return Err(std::io::Error::other("invalid image directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn inline_original(source: &str, budget: &mut usize) -> Result<(PathBuf, PathBuf), &'static str> {
    let root = original_store()?;
    inline_original_in(source, budget, &root)
}

pub(super) fn original_store() -> Result<PathBuf, &'static str> {
    let root = crate::state_dir()
        .map_err(|_| "original_storage_unavailable")?
        .join("message-images");
    private_dir(&root).map_err(|_| "original_storage_unavailable")?;
    let root = root.join("originals");
    private_dir(&root).map_err(|_| "original_storage_unavailable")?;
    std::fs::canonicalize(root).map_err(|_| "original_storage_unavailable")
}

fn inline_original_in(
    source: &str,
    budget: &mut usize,
    root: &Path,
) -> Result<(PathBuf, PathBuf), &'static str> {
    // The work allowance governs previews, never whether an original survives.
    *budget = budget.saturating_sub(source.len());
    let (header, encoded) = source.split_once(',').ok_or("invalid_inline_source")?;
    if !header.starts_with("data:") || !header.ends_with(";base64") {
        return Err("unsupported_inline_source");
    }
    private_dir(root).map_err(|_| "original_storage_unavailable")?;
    let staging = root.join(format!("{}.pending", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&staging)
            .map_err(|_| "original_storage_unavailable")?;
        let mut hasher = Sha256::new();
        let mut head = Vec::new();
        let mut size = 0u64;
        let mut chunks = encoded.as_bytes().chunks(8192).peekable();
        while let Some(chunk) = chunks.next() {
            if chunks.peek().is_some() && chunk.contains(&b'=') {
                return Err("invalid_inline_encoding");
            }
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .map_err(|_| "invalid_inline_encoding")?;
            size += decoded.len() as u64;
            if head.len() < crate::ASSET_SNIFF_BYTES {
                head.extend_from_slice(
                    &decoded[..decoded.len().min(crate::ASSET_SNIFF_BYTES - head.len())],
                );
            }
            hasher.update(&decoded);
            file.write_all(&decoded)
                .map_err(|_| "original_storage_unavailable")?;
        }
        if size == 0 {
            return Err("empty_inline_source");
        }
        let extension =
            crate::platform::uploads::sniff_upload_kind(&head).map_or("bin", |kind| kind.extension);
        let hash = hex::encode(hasher.finalize());
        file.flush().map_err(|_| "original_storage_unavailable")?;
        let path = publish_original(&staging, root, &hash, extension, size)?;
        Ok((path, root.to_path_buf()))
    })();
    let _ = std::fs::remove_file(staging);
    result
}

pub(super) fn publish_original(
    staging: &Path,
    root: &Path,
    hash: &str,
    extension: &str,
    size: u64,
) -> Result<PathBuf, &'static str> {
    let path = root.join(format!("{hash}.{extension}"));
    match std::fs::hard_link(staging, &path) {
        Ok(()) => {
            // Complete publication includes unlinking the staging name before
            // a version records this immutable inode's final metadata.
            std::fs::remove_file(staging).map_err(|_| "original_storage_unavailable")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Verify an existing digest-named inode with a bounded buffer.
            let mut existing =
                open_fenced(&path, root).map_err(|_| "original_storage_unavailable")?;
            if existing
                .metadata()
                .map_err(|_| "original_storage_unavailable")?
                .len()
                != size
            {
                return Err("original_storage_conflict");
            }
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 8192];
            loop {
                let read = existing
                    .read(&mut buffer)
                    .map_err(|_| "original_storage_unavailable")?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            if hex::encode(hasher.finalize()) != hash {
                return Err("original_storage_conflict");
            }
        }
        Err(_) => return Err("original_storage_unavailable"),
    }
    Ok(path)
}

/// Open an authorized inode, not a path subsequently re-opened after checking.
/// O_NOFOLLOW applies to the leaf; canonical equality rejects parent/leaf swaps
/// detectable across the open. Same-user shell access is not an OS sandbox.
pub(super) fn open_fenced(path: &Path, root: &Path) -> std::io::Result<std::fs::File> {
    let canonical = std::fs::canonicalize(path)?;
    if canonical != path || canonical == root || !canonical.starts_with(root) {
        return Err(std::io::Error::other("image outside scope"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    let current = std::fs::metadata(&canonical)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if opened.dev() != current.dev() || opened.ino() != current.ino() {
            return Err(std::io::Error::other("image identity changed"));
        }
    }
    if !opened.is_file() || std::fs::canonicalize(path)? != canonical {
        return Err(std::io::Error::other("invalid image file"));
    }
    Ok(file)
}

struct Normalized {
    source: String,
    asset: Option<MessageImageAsset>,
    served: Option<ServedImage>,
}

/// Some providers retain both typed content and its legacy joined-text view.
/// Only synchronize that view when exact equality proves it came from these
/// content entries. Never search/replace unrelated tool output or prose.
fn joined_output_matches(output: Option<&Value>, content: &[Value]) -> bool {
    let Some(mut remaining) = output.and_then(Value::as_str) else {
        return false;
    };
    for (index, part) in content.iter().enumerate() {
        if index != 0 {
            let Some(rest) = remaining.strip_prefix('\n') else {
                return false;
            };
            remaining = rest;
        }
        let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| part.as_str())
            .or_else(|| {
                (part["type"] == "file")
                    .then(|| part["uri"].as_str())
                    .flatten()
            })
        else {
            return false;
        };
        let Some(rest) = remaining.strip_prefix(text) else {
            return false;
        };
        remaining = rest;
    }
    !content.is_empty() && remaining.is_empty()
}

#[derive(Clone, Copy)]
enum FileScope {
    Session,
    AccountAttachment,
    AccountImageView,
}

fn normalize(
    asid: &str,
    source: String,
    root: Option<&Path>,
    scope: FileScope,
    budget: &mut usize,
    originals_dir: Option<&Path>,
) -> Normalized {
    let unchanged = || Normalized {
        source: source.clone(),
        asset: None,
        served: None,
    };
    if source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("/api/")
    {
        return unchanged();
    }
    let failed = |reason: &str| Normalized {
        source: source.clone(),
        asset: Some(MessageImageAsset::unavailable(&source, reason)),
        served: None,
    };
    let resolved = if source.starts_with("data:") {
        let stored = match originals_dir {
            Some(dir) => inline_original_in(&source, budget, dir),
            None => inline_original(&source, budget),
        };
        match stored {
            Ok(resolved) => Some(resolved),
            Err(reason) => return failed(reason),
        }
    } else {
        root.and_then(|root| {
            if matches!(
                scope,
                FileScope::AccountAttachment | FileScope::AccountImageView
            ) {
                super::account_file(&source, root)
            } else {
                hosted_file(&source, root)
            }
        })
        .or_else(|| {
            let uploads = crate::platform::uploads::uploads_dir().ok()?;
            let uploads = std::fs::canonicalize(uploads).ok()?;
            let path = super::resolve_image_path(&source, &uploads)?;
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            (!crate::platform::uploads::upload_expired(modified, std::time::SystemTime::now()))
                .then_some((path, uploads))
        })
    };
    let Some((path, fence)) = resolved else {
        return failed("reference_not_authorized_or_unavailable");
    };
    let Ok(mut file) = open_fenced(&path, &fence) else {
        return failed("original_read_unavailable");
    };
    let mut head = vec![0; crate::ASSET_SNIFF_BYTES];
    let read = file.read(&mut head).unwrap_or(0);
    head.truncate(read);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let kind = crate::sniff_asset_type(&head, &name);
    let id = format!(
        "ai_{}",
        digest(format!("{asid}\n{}", path.display()).as_bytes())
    );
    let url = format!("/api/agent-images/{id}/original");
    let (width, height) = image_dimensions(&head).unzip();
    let image = kind.kind == AssetKind::Image;
    // A native image-view item can explicitly reference account artifacts,
    // but it must not turn a credential/non-image file into a downloadable asset.
    if matches!(scope, FileScope::AccountImageView) && !image {
        return failed("image_content_unavailable");
    }
    let asset = MessageImageAsset {
        src: url.clone(),
        asset_id: id.clone(),
        url: url.clone(),
        mime: kind.mime.into(),
        width,
        height,
        thumbnail_url: image.then(|| format!("/api/agent-images/{id}/thumbnail")),
        preview_url: image.then(|| format!("/api/agent-images/{id}/preview")),
        download_url: Some(format!("/api/agent-images/{id}/download")),
        version_url: Some(format!("/api/agent-images/{id}/version")),
        size: file.metadata().ok().map(|metadata| metadata.len()),
        unavailable_reason: None,
    };
    Normalized {
        source: url,
        asset: Some(asset),
        served: Some(ServedImage { path, root: fence }),
    }
}

/// Only typed file content is examined. Inputs/output prose and filename-like
/// strings are never guessed to be tool images.
pub(super) async fn attach(
    state: &AppState,
    asid: &str,
    root: Option<PathBuf>,
    items: &mut [TimelineItem],
) {
    attach_with_originals_dir(state, asid, root, items, None).await;
}

async fn attach_with_originals_dir(
    state: &AppState,
    asid: &str,
    root: Option<PathBuf>,
    items: &mut [TimelineItem],
    originals_dir: Option<PathBuf>,
) {
    let mut inputs = Vec::new();
    let mut mirrored_outputs = Vec::new();
    for (index, item) in items.iter_mut().enumerate() {
        if let Some(sources) = item.attachments.take() {
            let sources = sources
                .into_iter()
                .map(|source| {
                    let label = item.image_assets.as_ref().is_some_and(|assets| {
                        assets.iter().any(|asset| {
                            asset.src == source
                                && asset.unavailable_reason.as_deref()
                                    == Some("attachment_name_only")
                        })
                    });
                    (source, label)
                })
                .collect::<Vec<_>>();
            let user_attachment = item.role == crate::agents::domain::TimelineRole::User
                && matches!(&item.part, AgentPart::Text { .. });
            inputs.push((
                index,
                None,
                sources,
                if user_attachment {
                    FileScope::AccountAttachment
                } else {
                    FileScope::Session
                },
            ));
        }
        if let AgentPart::Tool(call) = &mut item.part {
            // Only the native structured imageView's exact file gets the same
            // explicit account-artifact roots as a paired device file open.
            // Arbitrary output text and other tool files retain their session fence.
            let viewed_uri = (call.name == "image_view"
                && call
                    .metadata
                    .as_ref()
                    .is_some_and(|m| m["type"] == "imageView" && m["path"] == call.input["path"]))
            .then(|| {
                call.input["path"]
                    .as_str()
                    .and_then(|p| reqwest::Url::from_file_path(p).ok())
            })
            .flatten();
            if let Some(Value::Array(content)) = &call.content {
                let raw = call.output.as_ref() == call.content.as_ref();
                let has_file = content
                    .iter()
                    .any(|part| part["type"] == "file" && part["uri"].is_string());
                if has_file && (raw || joined_output_matches(call.output.as_ref(), content)) {
                    call.output = None;
                    mirrored_outputs.push((index, raw));
                }
            }
            if let Some(Value::Array(content)) = &mut call.content {
                for (position, part) in content.iter_mut().enumerate() {
                    if part["type"] == "file" {
                        if let Some(uri) = part
                            .get_mut("uri")
                            .and_then(|uri| uri.as_str().map(str::to_owned))
                        {
                            part["uri"] = Value::Null;
                            let scope = if viewed_uri.as_ref().is_some_and(|v| v.as_str() == uri) {
                                FileScope::AccountImageView
                            } else {
                                FileScope::Session
                            };
                            inputs.push((index, Some(position), vec![(uri, false)], scope));
                        }
                    }
                }
            }
        }
    }
    if inputs.is_empty() {
        return;
    }
    let Ok(permit) = ADMISSION.clone().acquire_owned().await else {
        return;
    };
    let asid = asid.to_owned();
    let Ok(outputs) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut budget = RESPONSE_WORK_BYTES;
        inputs
            .into_iter()
            .map(|(index, position, sources, scope)| {
                let sources = sources
                    .into_iter()
                    .map(|(source, label)| {
                        if label {
                            Normalized {
                                source,
                                asset: None,
                                served: None,
                            }
                        } else {
                            normalize(
                                &asid,
                                source,
                                root.as_deref(),
                                scope,
                                &mut budget,
                                originals_dir.as_deref(),
                            )
                        }
                    })
                    .collect::<Vec<_>>();
                (index, position, sources)
            })
            .collect::<Vec<_>>()
    })
    .await
    else {
        return;
    };
    for (index, position, sources) in outputs {
        let item = &mut items[index];
        let mut urls = Vec::new();
        for source in sources {
            if let Some(asset) = source.asset {
                if let Some(served) = source.served {
                    state
                        .message_images
                        .remember_served(&asset.asset_id, served);
                }
                if let Some(position) = position.filter(|_| asset.unavailable_reason.is_none()) {
                    if let AgentPart::Tool(call) = &mut item.part {
                        if let Some(Value::Array(content)) = &mut call.content {
                            content[position]["mime"] = Value::String(asset.mime.clone());
                        }
                    }
                }
                let assets = item.image_assets.get_or_insert_default();
                if !assets.iter().any(|existing| existing == &asset) {
                    assets.push(asset);
                }
            }
            urls.push(source.source);
        }
        if let Some(position) = position {
            if let AgentPart::Tool(call) = &mut item.part {
                if let Some(Value::Array(content)) = &mut call.content {
                    if let Some(url) = urls.into_iter().next() {
                        content[position]["uri"] = Value::String(url);
                    }
                }
            }
        } else {
            item.attachments.get_or_insert_default().extend(urls);
        }
    }
    for (index, raw) in mirrored_outputs {
        if let AgentPart::Tool(call) = &mut items[index].part {
            if let Some(Value::Array(content)) = &call.content {
                call.output = if raw {
                    call.content.clone()
                } else {
                    Some(Value::String(
                        content
                            .iter()
                            .filter_map(|part| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .or_else(|| part.as_str())
                                    .or_else(|| part.get("uri").and_then(Value::as_str))
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ))
                };
            }
        }
    }
}

fn unavailable() -> (StatusCode, axum::Json<Value>) {
    api_error(
        StatusCode::NOT_FOUND,
        "image_not_found",
        "image is unavailable",
    )
}

fn version_error(code: &'static str) -> (StatusCode, axum::Json<Value>) {
    let status = match code {
        "image_version_storage_full" => StatusCode::INSUFFICIENT_STORAGE,
        "original_storage_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::CONFLICT,
    };
    api_error(
        status,
        code,
        "original version unavailable; refresh the version descriptor",
    )
}

fn open_authorized(image: &ServedImage) -> Option<(std::fs::File, std::fs::Metadata)> {
    let file = open_fenced(&image.path, &image.root).ok()?;
    let metadata = file.metadata().ok()?;
    if crate::platform::uploads::uploads_dir()
        .ok()
        .and_then(|root| std::fs::canonicalize(root).ok())
        .is_some_and(|root| image.path.starts_with(root))
        && crate::platform::uploads::upload_expired(
            metadata.modified().ok()?,
            std::time::SystemTime::now(),
        )
    {
        return None;
    }
    Some((file, metadata))
}

/// Every derivative/original read rechecks original authorization first, even
/// if a derivative exists on disk. No unscoped path/query read endpoint.
pub(crate) async fn content(
    State(state): State<AppState>,
    RoutePath((id, variant)): RoutePath<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    original_response(state, id, variant, headers, None).await
}

pub(crate) async fn version_content(
    State(state): State<AppState>,
    RoutePath((id, variant, version)): RoutePath<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    original_response(state, id, variant, headers, Some(version)).await
}

async fn original_response(
    state: AppState,
    id: String,
    variant: String,
    headers: HeaderMap,
    version: Option<String>,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;
    if !matches!(
        variant.as_str(),
        "original" | "download" | "thumbnail" | "preview" | "version"
    ) || (version.is_some() && !matches!(variant.as_str(), "original" | "download"))
    {
        return Err(unavailable());
    }
    let image = state.message_images.served(&id).ok_or_else(unavailable)?;
    let permit = ADMISSION
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| unavailable())?;
    if variant == "version" {
        let acquired = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let (file, metadata) = open_authorized(&image).ok_or_else(unavailable)?;
            super::versions::acquire(&id, &image, file, metadata).map_err(version_error)
        })
        .await
        .map_err(|_| version_error("original_storage_unavailable"))??;
        let mut response = axum::Json(crate::content_envelope(
            serde_json::json!({"asset":acquired.descriptor()}),
        ))
        .into_response();
        response.headers_mut().insert(
            "cache-control",
            axum::http::HeaderValue::from_static("private, no-store"),
        );
        return Ok(response);
    }
    let fixed = version.is_some();
    let derivative = matches!(variant.as_str(), "thumbnail" | "preview");
    let preview = variant == "preview";
    let (file, metadata, mime, name, etag, integrity, rendered) =
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            if let Some(version) = version {
                // Scope/expiry is checked against the original before accessing an
                // already acquired immutable version. A version is not a bearer.
                open_authorized(&image).ok_or_else(unavailable)?;
                let selected = super::versions::selected(&id, &version).map_err(version_error)?;
                let (file, metadata) = selected.open().map_err(version_error)?;
                return Ok((
                    file,
                    metadata,
                    selected.mime,
                    selected.name.clone(),
                    selected.etag(),
                    selected.fingerprint,
                    None,
                ));
            }
            let legacy = (|| {
                let (mut file, metadata) = open_authorized(&image)?;
                let size = metadata.len();
                let mut head = Vec::new();
                (&mut file)
                    .take(crate::ASSET_SNIFF_BYTES as u64)
                    .read_to_end(&mut head)
                    .ok()?;
                file.rewind().ok()?;
                let kind = crate::sniff_asset_type(&head, image.path.file_name()?.to_str()?);
                let etag = metadata_etag(&metadata);
                let rendered = if derivative && size <= MAX_PREVIEW_SOURCE_BYTES {
                    let mut bytes = Vec::new();
                    (&mut file)
                        .take(MAX_PREVIEW_SOURCE_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .ok()?;
                    (bytes.len() as u64 == size)
                        .then(|| super::previews::derivative(&bytes, preview))
                        .flatten()
                } else {
                    None
                };
                let name =
                    crate::platform::assets::header_safe_name(image.path.file_name()?.to_str()?);
                Some((
                    file,
                    metadata,
                    kind.mime,
                    name,
                    etag.clone(),
                    etag,
                    rendered,
                ))
            })();
            legacy.ok_or_else(unavailable)
        })
        .await
        .map_err(|_| unavailable())??;
    if derivative {
        let rendered = rendered.ok_or_else(|| {
            api_error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "image_preview_unavailable",
                "image preview is unavailable; use the original",
            )
        })?;
        let etag = format!("\"{}\"", digest(&rendered));
        let matched = headers
            .get("if-none-match")
            .is_some_and(|value| value.as_bytes() == etag.as_bytes());
        let mut response = Response::builder()
            .status(if matched {
                StatusCode::NOT_MODIFIED
            } else {
                StatusCode::OK
            })
            .header("content-type", "image/webp")
            .header("etag", etag)
            .header("cache-control", "private, no-store");
        if !matched {
            response = response.header("content-length", rendered.len());
        }
        return response
            .body(if matched {
                // Axum otherwise synthesizes Content-Length: 0 from an exact
                // empty size hint, invalid for this nonempty representation.
                Body::from_stream(futures::stream::empty::<
                    Result<axum::body::Bytes, std::io::Error>,
                >())
            } else {
                Body::from(rendered)
            })
            .map_err(|_| unavailable());
    }
    let size = metadata.len();
    if fixed
        && (headers
            .get("if-match")
            .is_some_and(|value| value.as_bytes() != etag.as_bytes() && value.as_bytes() != b"*")
            || headers
                .get("if-range")
                .is_some_and(|value| value.as_bytes().starts_with(b"W/")))
    {
        return Err(api_error(
            StatusCode::PRECONDITION_FAILED,
            "image_version_precondition_failed",
            "original version precondition failed",
        ));
    }
    let not_modified = headers
        .get("if-none-match")
        .is_some_and(|value| value.as_bytes() == etag.as_bytes());
    if not_modified {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header("etag", &etag)
            .header("cache-control", "private, no-store")
            .body(Body::from_stream(futures::stream::empty::<
                Result<axum::body::Bytes, std::io::Error>,
            >()))
            .unwrap());
    }
    let range = headers.get("range").filter(|_| {
        headers
            .get("if-range")
            // A mutable workspace inode has only a weak validator. RFC If-Range
            // requires strong comparison; an explicit weak If-Range gets 200.
            .is_none_or(|value| !etag.starts_with("W/") && value.as_bytes() == etag.as_bytes())
    });
    let selected = range.map(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| byte_range(value, size))
    });
    if selected == Some(None) {
        return Ok(Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("content-range", format!("bytes */{size}"))
            .header("cache-control", "private, no-store")
            .body(Body::empty())
            .unwrap());
    }
    let (start, end) = selected.flatten().unwrap_or((0, size.saturating_sub(1)));
    let length = if size == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(start))
        .await
        .map_err(|_| unavailable())?;
    let expected = integrity;
    let body = async_stream::stream! {
        let mut remaining=length; let mut buffer=vec![0u8;crate::ASSET_CONTENT_CHUNK_BYTES];
        while remaining>0 {
            let count=remaining.min(buffer.len() as u64) as usize;
            match tokio::io::AsyncReadExt::read(&mut file,&mut buffer[..count]).await {
                Ok(0) => { yield Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof,"original changed during read")); break; },
                Ok(read) => {
                    if file.metadata().await.ok().is_none_or(|metadata| metadata_etag(&metadata)!=expected) {
                        yield Err(std::io::Error::other("original changed during read")); break;
                    }
                    remaining-=read as u64;
                    yield Ok(axum::body::Bytes::copy_from_slice(&buffer[..read]));
                },
                Err(error) => { yield Err(error); break; }
            }
        }
    };
    let mut response = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header("content-type", mime)
        .header("content-length", length)
        .header("etag", &etag)
        .header("accept-ranges", "bytes")
        .header("cache-control", "private, no-store")
        .header(
            "content-disposition",
            format!(
                "{}; filename=\"{name}\"",
                if variant == "download" {
                    "attachment"
                } else {
                    "inline"
                }
            ),
        );
    if range.is_some() {
        response = response.header("content-range", format!("bytes {start}-{end}/{size}"));
    }
    response
        .body(Body::from_stream(body))
        .map_err(|_| unavailable())
}

pub(super) fn metadata_etag(metadata: &std::fs::Metadata) -> String {
    let mut identity = format!("{}:{:?}", metadata.len(), metadata.modified().ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        identity.push_str(&format!(
            ":{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ));
    }
    format!("W/\"{}\"", digest(identity.as_bytes()))
}

fn byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    if size == 0 {
        return None;
    }
    let (start, end) = value.strip_prefix("bytes=")?.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(size);
        return (suffix > 0).then_some((size - suffix, size - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().ok()?.min(size - 1)
    };
    (start <= end && start < size).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{TimelineRole, ToolCall};
    use axum::http::HeaderValue;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn workspace() -> (Scratch, PathBuf) {
        let dir = crate::test_support::asset_test_dir("image-originals");
        let root = dir.join("work");
        std::fs::create_dir(&root).unwrap();
        (Scratch(dir), root)
    }
    pub(super) fn png(width: u32, height: u32, seed: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            let mut word = x.wrapping_mul(2654435761) ^ y.wrapping_mul(2246822519) ^ seed;
            word ^= word >> 13;
            word = word.wrapping_mul(3266489917);
            word ^= word >> 16;
            image::Rgb([word as u8, (word >> 8) as u8, (word >> 16) as u8])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
    fn item(sources: Vec<String>) -> TimelineItem {
        TimelineItem {
            id: "m:t0".into(),
            message_id: "m".into(),
            role: TimelineRole::User,
            part: AgentPart::Text {
                text: "images".into(),
            },
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: Some(sources),
            image_assets: None,
            generation: None,
        }
    }
    async fn read(state: &AppState, id: &str, variant: &str, headers: HeaderMap) -> Response {
        content(
            State(state.clone()),
            RoutePath((id.into(), variant.into())),
            headers,
        )
        .await
        .unwrap()
    }
    async fn bytes(response: Response) -> Vec<u8> {
        axum::body::to_bytes(response.into_body(), MAX_PREVIEW_SOURCE_BYTES as usize + 1)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn twelve_originals_stay_byte_exact_while_timeline_and_webp_are_small() {
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let mut originals = Vec::new();
        let mut paths = Vec::new();
        for index in 0..12 {
            let (width, height) = [(640, 480), (800, 200), (200, 800)][index % 3];
            let original = png(width, height, index as u32);
            let path = root.join(format!("image{index}.not-an-image-extension"));
            std::fs::write(&path, &original).unwrap();
            originals.push(original);
            paths.push(path.to_string_lossy().into_owned());
        }
        let mut items = vec![item(paths.clone())];
        attach(&state, "session-twelve", Some(root.clone()), &mut items).await;
        let wire = serde_json::to_string(&items).unwrap();
        assert!(wire.len() < 20_000);
        assert!(!wire.contains("base64") && !wire.contains("file://"));
        let assets = items[0].image_assets.as_ref().unwrap();
        assert_eq!(assets.len(), 12);
        assert_eq!(items[0].attachments.as_ref().unwrap().len(), 12);
        let mut thumbnail_bytes = 0;
        for (index, asset) in assets.iter().enumerate() {
            assert_eq!(asset.mime, "image/png");
            assert!(asset.url.ends_with("/original"));
            assert_eq!(items[0].attachments.as_ref().unwrap()[index], asset.url);
            let response = read(
                &state,
                &asset.asset_id,
                "thumbnail",
                crate::bearer_headers("token"),
            )
            .await;
            assert_eq!(response.headers()["content-type"], "image/webp");
            let thumbnail = bytes(response).await;
            let decoded =
                image::load_from_memory_with_format(&thumbnail, image::ImageFormat::WebP).unwrap();
            assert!(decoded.width() <= 384 && decoded.height() <= 384);
            assert_eq!(
                u64::from(decoded.width()) * u64::from(asset.height.unwrap()),
                u64::from(decoded.height()) * u64::from(asset.width.unwrap())
            );
            assert!(thumbnail.len() < originals[index].len());
            thumbnail_bytes += thumbnail.len();
            let response = read(
                &state,
                &asset.asset_id,
                "download",
                crate::bearer_headers("token"),
            )
            .await;
            assert!(response.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment;"));
            assert_eq!(digest(&bytes(response).await), digest(&originals[index]));
            assert_eq!(std::fs::read(&paths[index]).unwrap(), originals[index]);
        }
        assert!(thumbnail_bytes < originals.iter().map(Vec::len).sum::<usize>() / 2);
        eprintln!("synthetic 12-image evidence: originals={} bytes, timeline={} bytes, thumbnails={} bytes", originals.iter().map(Vec::len).sum::<usize>(), wire.len(), thumbnail_bytes);
        let asset = &assets[0];
        let original = read(
            &state,
            &asset.asset_id,
            "original",
            crate::bearer_headers("token"),
        )
        .await;
        let etag = original.headers()["etag"].clone();
        assert_eq!(bytes(original).await, originals[0]);
        for (range, expected) in [
            ("bytes=1-15", originals[0][1..16].to_vec()),
            (
                "bytes=-10",
                originals[0][originals[0].len() - 10..].to_vec(),
            ),
        ] {
            let mut headers = crate::bearer_headers("token");
            headers.insert("range", HeaderValue::from_str(range).unwrap());
            let response = read(&state, &asset.asset_id, "download", headers).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(bytes(response).await, expected);
        }
        let mut headers = crate::bearer_headers("token");
        headers.insert("if-none-match", etag);
        assert_eq!(
            read(&state, &asset.asset_id, "original", headers)
                .await
                .status(),
            StatusCode::NOT_MODIFIED
        );
        let mut headers = crate::bearer_headers("token");
        headers.insert("range", HeaderValue::from_static("bytes=999999999-"));
        assert_eq!(
            read(&state, &asset.asset_id, "download", headers)
                .await
                .status(),
            StatusCode::RANGE_NOT_SATISFIABLE
        );
        attach(&state, "session-twelve", Some(root), &mut items).await;
        assert_eq!(items[0].image_assets.as_ref().unwrap().len(), 12);
    }

    #[tokio::test]
    async fn inline_original_is_private_immutable_and_scoped_without_a_workspace() {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let original = png(900, 900, 139);
        assert!(original.len() > crate::agents::attachment_images::MAX_IMAGE_BYTES);
        let source = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&original)
        );
        let mut first = vec![item(vec![source.clone()])];
        let mut second = vec![item(vec![source])];
        let ((), ()) = tokio::join!(
            attach(&state, "session-inline-a", None, &mut first),
            attach(&state, "session-inline-b", None, &mut second)
        );
        let asset = &first[0].image_assets.as_ref().unwrap()[0];
        let other = &second[0].image_assets.as_ref().unwrap()[0];
        assert_ne!(asset.asset_id, other.asset_id);
        assert_eq!(
            bytes(
                read(
                    &state,
                    &asset.asset_id,
                    "download",
                    crate::bearer_headers("token")
                )
                .await
            )
            .await,
            original
        );
        let stored = state.message_images.served(&asset.asset_id).unwrap();
        assert_eq!(
            stored.path,
            state.message_images.served(&other.asset_id).unwrap().path
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&stored.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let before = std::fs::metadata(&stored.path).unwrap().modified().unwrap();
        let preview = bytes(
            read(
                &state,
                &asset.asset_id,
                "preview",
                crate::bearer_headers("token"),
            )
            .await,
        )
        .await;
        let decoded_preview =
            image::load_from_memory_with_format(&preview, image::ImageFormat::WebP).unwrap();
        assert_eq!(
            (decoded_preview.width(), decoded_preview.height()),
            (900, 900)
        );
        let calls = (0..8).map(|_| {
            read(
                &state,
                &asset.asset_id,
                "thumbnail",
                crate::bearer_headers("token"),
            )
        });
        for result in futures::future::join_all(calls).await {
            assert_eq!(result.headers()["content-type"], "image/webp");
        }
        assert_eq!(
            std::fs::metadata(&stored.path).unwrap().modified().unwrap(),
            before
        );
        let metadata = std::fs::metadata(&stored.path).unwrap();
        assert_eq!(metadata.len(), original.len() as u64);
        assert_eq!(
            content(
                State(state.clone()),
                RoutePath((asset.asset_id.clone(), "original".into())),
                HeaderMap::new()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            content(
                State(state.clone()),
                RoutePath((asset.asset_id.clone(), "original".into())),
                crate::bearer_headers("bad-token")
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            content(
                State(state),
                RoutePath(("ai_unknown".into(), "original".into())),
                crate::bearer_headers("token")
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn structured_user_cache_images_get_previews_but_assistant_paths_do_not() {
        let (scratch, root) = workspace();
        let path = scratch.0.join("clipboard.png");
        let original = png(20, 30, 42);
        std::fs::write(&path, &original).unwrap();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let mut rows = vec![item(vec![path.to_string_lossy().into_owned()])];
        attach(&state, "user", Some(root.clone()), &mut rows).await;
        let asset = &rows[0].image_assets.as_ref().unwrap()[0];
        assert!(asset.unavailable_reason.is_none());
        assert_eq!(rows[0].attachments.as_ref().unwrap()[0], asset.src);
        assert_eq!(asset.mime, "image/png");
        assert_eq!(
            bytes(
                read(
                    &state,
                    &asset.asset_id,
                    "original",
                    crate::bearer_headers("token")
                )
                .await
            )
            .await,
            original
        );
        let thumbnail = bytes(
            read(
                &state,
                &asset.asset_id,
                "thumbnail",
                crate::bearer_headers("token"),
            )
            .await,
        )
        .await;
        assert_eq!(image::load_from_memory(&thumbnail).unwrap().width(), 20);
        let mut assistant = vec![item(vec![path.to_string_lossy().into_owned()])];
        assistant[0].role = TimelineRole::Assistant;
        attach(&state, "assistant", Some(root), &mut assistant).await;
        assert_eq!(
            assistant[0].image_assets.as_ref().unwrap()[0]
                .unavailable_reason
                .as_deref(),
            Some("reference_not_authorized_or_unavailable")
        );
        #[cfg(unix)]
        {
            // /etc is outside the account, cache/temp and workspace roots.
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", &path).unwrap();
            assert_eq!(
                content(
                    State(state),
                    RoutePath((asset.asset_id.clone(), "thumbnail".into())),
                    crate::bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn typed_tool_files_use_actual_mime_and_never_read_prose_or_escape_the_scope() {
        let (scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let path = root.join("diagram.pdf");
        std::fs::write(&path, png(640, 480, 881)).unwrap();
        let outside = scratch.0.join("outside.png");
        std::fs::write(&outside, png(10, 10, 4)).unwrap();
        let call: ToolCall = serde_json::from_value(serde_json::json!({"id":"tool","name":"any_provider_tool","input":{"path":outside},"state":"completed","status":"completed","content":[{"type":"file","uri":path,"mime":"application/pdf"}]})).unwrap();
        let mut items = vec![item(vec![
            outside.to_string_lossy().into_owned(),
            "../outside.png".into(),
        ])];
        items[0].part = AgentPart::Tool(call);
        attach(&state, "tool-session", Some(root.clone()), &mut items).await;
        assert_eq!(items[0].attachments.as_ref().unwrap().len(), 2);
        assert!(items[0]
            .image_assets
            .as_ref()
            .unwrap()
            .iter()
            .any(|asset| asset.unavailable_reason.is_some()));
        let asset = items[0]
            .image_assets
            .as_ref()
            .unwrap()
            .iter()
            .find(|asset| asset.unavailable_reason.is_none())
            .unwrap();
        assert_eq!(asset.mime, "image/png");
        let AgentPart::Tool(call) = &items[0].part else {
            panic!()
        };
        assert_eq!(call.content.as_ref().unwrap()[0]["mime"], "image/png");
        let thumb1 = bytes(
            read(
                &state,
                &asset.asset_id,
                "thumbnail",
                crate::bearer_headers("token"),
            )
            .await,
        )
        .await;
        std::fs::write(&path, png(200, 800, 882)).unwrap();
        let thumb2 = bytes(
            read(
                &state,
                &asset.asset_id,
                "thumbnail",
                crate::bearer_headers("token"),
            )
            .await,
        )
        .await;
        assert_ne!(digest(&thumb1), digest(&thumb2));
        assert_eq!(image::load_from_memory(&thumb2).unwrap().height(), 384);
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
            for variant in ["original", "download", "thumbnail", "preview"] {
                assert_eq!(
                    content(
                        State(state.clone()),
                        RoutePath((asset.asset_id.clone(), variant.into())),
                        crate::bearer_headers("token")
                    )
                    .await
                    .unwrap_err()
                    .0,
                    StatusCode::NOT_FOUND
                );
            }
        }
    }

    #[tokio::test]
    async fn unsupported_and_huge_images_keep_original_access_but_never_fake_a_preview() {
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let mut huge = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        huge.extend(100_000u32.to_be_bytes());
        huge.extend(100_000u32.to_be_bytes());
        huge.extend([8, 6, 0, 0, 0]);
        for (name, original) in [
            ("animated.gif", b"GIF89a\x01\x00\x01\x00".to_vec()),
            ("report.pdf", b"%PDF-1.7\nfixture".to_vec()),
            ("huge.png", huge),
        ] {
            let path = root.join(name);
            std::fs::write(&path, &original).unwrap();
            let mut items = vec![item(vec![path.to_string_lossy().into_owned()])];
            attach(&state, "unsupported", Some(root.clone()), &mut items).await;
            let asset = &items[0].image_assets.as_ref().unwrap()[0];
            assert_eq!(
                bytes(
                    read(
                        &state,
                        &asset.asset_id,
                        "download",
                        crate::bearer_headers("token")
                    )
                    .await
                )
                .await,
                original
            );
            assert_eq!(
                content(
                    State(state.clone()),
                    RoutePath((asset.asset_id.clone(), "thumbnail".into())),
                    crate::bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
                assert!(content(
                    State(state.clone()),
                    RoutePath((asset.asset_id.clone(), "original".into())),
                    crate::bearer_headers("token")
                )
                .await
                .is_err());
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
    }

    #[test]
    fn ranges_are_single_bounded_byte_intervals() {
        assert_eq!(byte_range("bytes=1-100", 10), Some((1, 9)));
        assert_eq!(byte_range("bytes=5-", 10), Some((5, 9)));
        assert_eq!(byte_range("bytes=-100", 10), Some((0, 9)));
        for value in [
            "bytes=-0",
            "bytes=10-",
            "bytes=2-1",
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=18446744073709551616-",
        ] {
            assert_eq!(byte_range(value, 10), None);
        }
        assert_eq!(byte_range("bytes=0-", 0), None);
    }

    #[tokio::test]
    async fn real_provider_projections_export_original_references_without_duplicate_base64() {
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let original = png(900, 900, 998);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
        let source = format!("data:image/png;base64,{encoded}");
        let asid = crate::agents::domain::AgentSessionId("provider-image".into());
        let pdf = b"%PDF-1.7\ninline original fixture";
        let message = serde_json::json!({"id":"user","type":"user","text":"[Image 1] Keep this raw label", "files":[{"source":{"type":"inline"},"mime":"image/png","data":encoded},{"source":{"type":"inline"},"mime":"application/pdf","data":base64::engine::general_purpose::STANDARD.encode(pdf)}]});
        let mut opencode = crate::agents::adapters::opencode::mapper::map_message(&message, &asid);
        super::super::attach_to_items(&state, &asid.0, root.to_str(), &mut opencode).await;
        assert!(!serde_json::to_string(&opencode).unwrap().contains("base64"));
        assert!(
            matches!(&opencode[0].part,AgentPart::Text{text} if text=="[Image 1] Keep this raw label")
        );
        assert!(opencode[0].attachments.as_ref().unwrap()[0].ends_with("/original"));
        let pdf_asset = &opencode[0].image_assets.as_ref().unwrap()[1];
        assert_eq!(pdf_asset.mime, "application/pdf");
        assert!(pdf_asset.thumbnail_url.is_none());
        assert_eq!(
            bytes(
                read(
                    &state,
                    &pdf_asset.asset_id,
                    "download",
                    crate::bearer_headers("token")
                )
                .await
            )
            .await,
            pdf
        );
        for explicit_output in [false, true] {
            let mut value = serde_json::json!({"id":"read","name":"read","state":{"status":"completed","input":{"path":"not inferred"},"content":[{"type":"text","text":"Image follows"},{"type":"file","uri":source,"mime":"image/png"}]}});
            if explicit_output {
                value["state"]["output"] = serde_json::json!("Unrelated raw tool prose");
            }
            let call = crate::agents::adapters::opencode::mapper::map_tool_call(&value);
            let mut items = vec![item(Vec::new())];
            items[0].part = AgentPart::Tool(call);
            super::super::attach_to_items(&state, &asid.0, root.to_str(), &mut items).await;
            let wire = serde_json::to_string(&items).unwrap();
            assert!(!wire.contains("base64"));
            let AgentPart::Tool(call) = &items[0].part else {
                panic!()
            };
            if explicit_output {
                assert_eq!(
                    call.output,
                    Some(serde_json::json!("Unrelated raw tool prose"))
                );
            } else {
                assert!(call
                    .output
                    .as_ref()
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("/original"));
            }
        }
        let path = root.join("view.png");
        std::fs::write(&path, &original).unwrap();
        let mut codex = crate::agents::adapters::codex::mapper::item(
            "thread",
            "turn",
            &serde_json::json!({"id":"view","type":"imageView","path":path}),
            0,
        );
        super::super::attach_to_items(&state, "thread", root.to_str(), &mut codex).await;
        assert_eq!(codex[0].image_assets.as_ref().unwrap()[0].mime, "image/png");
        let mut generated = crate::agents::adapters::codex::mapper::item(
            "thread",
            "turn",
            &serde_json::json!({"id":"generated","type":"imageGeneration","status":"completed","result":encoded}),
            0,
        );
        super::super::attach_to_items(&state, "thread", root.to_str(), &mut generated).await;
        assert!(!serde_json::to_string(&generated)
            .unwrap()
            .contains("base64"));
        let asset = &generated[0].image_assets.as_ref().unwrap()[0];
        assert_eq!(
            bytes(
                read(
                    &state,
                    &asset.asset_id,
                    "download",
                    crate::bearer_headers("token")
                )
                .await
            )
            .await,
            original
        );
    }

    #[tokio::test]
    async fn cached_upload_preview_cannot_bypass_existing_upload_retention() {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let uploads = crate::platform::uploads::ensure_uploads_dir().unwrap();
        let path = uploads.join(format!("{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, png(100, 100, 774)).unwrap();
        let mut items = vec![item(vec![path.to_string_lossy().into_owned()])];
        attach(&state, "upload-retention", None, &mut items).await;
        let id = &items[0].image_assets.as_ref().unwrap()[0].asset_id;
        assert_eq!(
            read(&state, id, "thumbnail", crate::bearer_headers("token"))
                .await
                .status(),
            StatusCode::OK
        );
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now()
                    - crate::platform::uploads::UPLOAD_RETENTION
                    - std::time::Duration::from_secs(1),
            )
            .unwrap();
        for variant in ["original", "download", "thumbnail", "preview"] {
            assert_eq!(
                content(
                    State(state.clone()),
                    RoutePath((id.clone(), variant.into())),
                    crate::bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::NOT_FOUND
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    async fn streamed_hash(response: Response) -> (String, u64) {
        use futures::StreamExt as _;
        let mut stream = response.into_body().into_data_stream();
        let mut hash = Sha256::new();
        let mut count = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            assert!(chunk.len() <= crate::ASSET_CONTENT_CHUNK_BYTES);
            count += chunk.len() as u64;
            hash.update(&chunk);
        }
        (hex::encode(hash.finalize()), count)
    }

    #[tokio::test]
    async fn mapper_to_egress_preserves_two_over_32_mib_inline_originals_after_preview_budget_exhaustion(
    ) {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let mut original = png(16, 16, 519);
        original.resize(33 * 1024 * 1024, 0);
        let expected = digest(&original);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
        drop(original);
        let messages=(0..2).map(|id| serde_json::json!({"id":format!("large{id}"),"type":"user","text":"Large original","files":[{"source":{"type":"inline"},"mime":"image/png","data":encoded}]})).collect::<Vec<_>>();
        drop(encoded);
        let asid = crate::agents::domain::AgentSessionId("large-inline".into());
        let mut items =
            crate::agents::adapters::opencode::mapper::map_messages_to_timeline(&messages, &asid);
        drop(messages);
        attach(&state, &asid.0, None, &mut items).await;
        assert!(serde_json::to_vec(&items).unwrap().len() < 10_000);
        for item in &items {
            let asset = &item.image_assets.as_ref().unwrap()[0];
            assert!(asset.unavailable_reason.is_none());
            assert_eq!(asset.size, Some(33 * 1024 * 1024));
            assert!(item.attachments.as_ref().unwrap()[0].ends_with("/original"));
            assert_eq!(
                content(
                    State(state.clone()),
                    RoutePath((asset.asset_id.clone(), "thumbnail".into())),
                    crate::bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::UNSUPPORTED_MEDIA_TYPE
            );
            let response = read(
                &state,
                &asset.asset_id,
                "download",
                crate::bearer_headers("token"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                streamed_hash(response).await,
                (expected.clone(), 33 * 1024 * 1024)
            );
            let mut headers = crate::bearer_headers("token");
            headers.insert("range", HeaderValue::from_static("bytes=34602990-34603007"));
            let response = read(&state, &asset.asset_id, "original", headers).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(bytes(response).await, vec![0; 18]);
        }
    }

    #[tokio::test]
    async fn mapper_to_egress_keeps_more_than_32_references_and_never_opens_a_name_only_label() {
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let mut files = Vec::new();
        for id in 0..40 {
            let path = root.join(format!("{id}.png"));
            std::fs::write(&path, png(10, 10, id)).unwrap();
            files.push(serde_json::json!({"uri":path,"mime":"image/png"}));
        }
        std::fs::write(root.join("name-only.png"), png(10, 10, 44)).unwrap();
        files.push(serde_json::json!({"name":"name-only.png"}));
        let asid = crate::agents::domain::AgentSessionId("many-files".into());
        let mut items = crate::agents::adapters::opencode::mapper::map_message(
            &serde_json::json!({"id":"many","type":"user","text":"All files","files":files}),
            &asid,
        );
        attach(&state, &asid.0, Some(root.clone()), &mut items).await;
        let sources = items[0].attachments.as_ref().unwrap();
        assert_eq!(sources.len(), 41);
        assert_eq!(sources[40], "name-only.png");
        let assets = items[0].image_assets.as_ref().unwrap();
        assert_eq!(
            assets
                .iter()
                .filter(|asset| asset.unavailable_reason.is_none())
                .count(),
            40
        );
        assert!(assets.iter().any(|asset| asset.src == "name-only.png"
            && asset.unavailable_reason.as_deref() == Some("attachment_name_only")
            && asset.url.is_empty()));
        let guessed = format!(
            "ai_{}",
            digest(format!("{}\n{}", asid.0, root.join("name-only.png").display()).as_bytes())
        );
        assert!(state.message_images.served(&guessed).is_none());
        for asset in assets
            .iter()
            .filter(|asset| asset.unavailable_reason.is_none())
        {
            assert_eq!(
                read(
                    &state,
                    &asset.asset_id,
                    "download",
                    crate::bearer_headers("token")
                )
                .await
                .status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn mapper_to_egress_storage_failure_retains_raw_original_and_explicit_failure_without_fake_url(
    ) {
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let blocked = root.join("not-a-directory");
        std::fs::write(&blocked, b"owned fixture").unwrap();
        let original = png(32, 32, 82);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
        let asid = crate::agents::domain::AgentSessionId("storage-failure".into());
        let mut items = crate::agents::adapters::opencode::mapper::map_message(
            &serde_json::json!({"id":"failure","type":"user","text":"Original","files":[{"source":{"type":"inline"},"mime":"image/png","data":encoded}]}),
            &asid,
        );
        let source = items[0].attachments.as_ref().unwrap()[0].clone();
        let call = crate::agents::adapters::opencode::mapper::map_tool_call(
            &serde_json::json!({"id":"tool-failure","name":"read","state":{"status":"completed","content":[{"type":"file","uri":source,"mime":"image/png"}]}}),
        );
        let mut tool = item(Vec::new());
        tool.part = AgentPart::Tool(call);
        items.push(tool);
        attach_with_originals_dir(&state, &asid.0, Some(root), &mut items, Some(blocked)).await;
        assert_eq!(items[0].attachments.as_ref().unwrap()[0], source);
        let raw = base64::engine::general_purpose::STANDARD
            .decode(source.split_once(',').unwrap().1)
            .unwrap();
        assert_eq!(raw, original);
        let error = &items[0].image_assets.as_ref().unwrap()[0];
        assert_eq!(
            error.unavailable_reason.as_deref(),
            Some("original_storage_unavailable")
        );
        assert!(error.url.is_empty() && error.asset_id.is_empty() && error.download_url.is_none());
        let AgentPart::Tool(call) = &items[1].part else {
            panic!()
        };
        assert_eq!(call.content.as_ref().unwrap()[0]["type"], "file");
        assert_eq!(call.content.as_ref().unwrap()[0]["uri"], source);
        assert_eq!(
            call.output.as_ref().unwrap().as_str(),
            Some(source.as_str())
        );
        assert_eq!(
            items[1].image_assets.as_ref().unwrap()[0]
                .unavailable_reason
                .as_deref(),
            Some("original_storage_unavailable")
        );
    }

    #[tokio::test]
    async fn mapper_to_egress_untrusted_mime_preserves_original_bytes_and_explicit_failures() {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let original = png(40, 30, 938);
        let expected = digest(&original);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
        let variants = [
            None,
            Some(String::new()),
            Some("image/png; charset=binary".into()),
            Some("image/png\r\nX-Injected: value".into()),
            Some("image//png".into()),
            Some("image/".into()),
            Some("not-a-mime".into()),
            Some("x".repeat(1024)),
            Some("图/png".into()),
        ];
        let asid = crate::agents::domain::AgentSessionId("untrusted-mime".into());
        for (index, mime) in variants.into_iter().enumerate() {
            let mut file = serde_json::json!({"source":{"type":"inline"},"name":"label-is-not-a-path.png","data":encoded});
            if let Some(mime) = mime {
                file["mime"] = Value::String(mime);
            }
            let message = serde_json::json!({"id":format!("mime-{index}"),"type":"user","text":"Preserve bytes","files":[file]});
            let mut items = crate::agents::adapters::opencode::mapper::map_message(&message, &asid);
            let source = &items[0].attachments.as_ref().unwrap()[0];
            assert_eq!(
                source.split_once(',').unwrap().0,
                "data:application/octet-stream;base64"
            );
            attach(&state, &asid.0, None, &mut items).await;
            let asset = &items[0].image_assets.as_ref().unwrap()[0];
            assert!(asset.unavailable_reason.is_none());
            assert_eq!(asset.mime, "image/png");
            let response = read(
                &state,
                &asset.asset_id,
                "download",
                crate::bearer_headers("token"),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(digest(&bytes(response).await), expected);
        }
        // Same unsafe MIME with broken encoding retains raw data and a real
        // egress failure, not a name-only surrogate or invented original URL.
        let message = serde_json::json!({"id":"mime-failure","type":"user","text":"Original","files":[{"source":{"type":"inline"},"mime":"image/png; injected=value","name":"not-a-path.png","data":"!!!!"}]});
        let mut items = crate::agents::adapters::opencode::mapper::map_message(&message, &asid);
        attach(&state, &asid.0, None, &mut items).await;
        assert_eq!(
            items[0].attachments.as_ref().unwrap()[0],
            "data:application/octet-stream;base64,!!!!"
        );
        let failure = &items[0].image_assets.as_ref().unwrap()[0];
        assert_eq!(
            failure.unavailable_reason.as_deref(),
            Some("invalid_inline_encoding")
        );
        assert!(
            failure.url.is_empty() && failure.asset_id.is_empty() && failure.download_url.is_none()
        );
    }

    #[tokio::test]
    async fn preview_conditional_get_omits_304_content_length_and_preserves_200_length() {
        use tower::ServiceExt as _;
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let source = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png(100, 80, 939))
        );
        let mut items = vec![item(vec![source])];
        attach(&state, "conditional-preview", None, &mut items).await;
        let id = &items[0].image_assets.as_ref().unwrap()[0].asset_id;
        let app = axum::Router::new()
            .route(
                "/api/agent-images/{id}/{variant}",
                axum::routing::get(content),
            )
            .with_state(state);
        for variant in ["thumbnail", "preview", "original"] {
            let uri = format!("/api/agent-images/{id}/{variant}");
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("GET")
                        .uri(&uri)
                        .header("authorization", "Bearer token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let etag = response.headers()["etag"].clone();
            let length = response.headers()["content-length"]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let body = bytes(response).await;
            assert!(!body.is_empty());
            assert_eq!(length, body.len());
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("GET")
                        .uri(&uri)
                        .header("authorization", "Bearer token")
                        .header("if-none-match", etag.clone())
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()["etag"], etag);
            assert!(!response.headers().contains_key("content-length"));
            assert!(bytes(response).await.is_empty());
        }
    }

    #[tokio::test]
    async fn existing_28_mib_video_keeps_legacy_access_and_streamed_new_download_ranges() {
        use axum::extract::Query;
        let (_scratch, root) = workspace();
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let path = root.join("large.mp4");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"\0\0\0\x20ftypisom\0\0\0\0").unwrap();
        file.set_len(28 * 1024 * 1024).unwrap();
        drop(file);
        state
            .message_images
            .remember_root("video-large", root.to_str());
        let metadata = super::super::file_asset(
            State(state.clone()),
            RoutePath("video-large".into()),
            Query(super::super::AudioAssetQuery {
                uri: path.to_string_lossy().into_owned(),
            }),
            crate::bearer_headers("token"),
        )
        .await
        .unwrap();
        let asset = &metadata.0["data"]["asset"];
        let id = asset["id"].as_str().unwrap();
        let old = crate::platform::assets::asset_download(
            State(state.clone()),
            RoutePath(id.into()),
            crate::bearer_headers("token"),
        )
        .await
        .unwrap();
        assert_eq!(old.status(), StatusCode::OK);
        let old_hash = streamed_hash(old).await;
        let new = read(&state, id, "download", crate::bearer_headers("token")).await;
        assert_eq!(new.status(), StatusCode::OK);
        assert_eq!(streamed_hash(new).await, old_hash);
        let mut headers = crate::bearer_headers("token");
        headers.insert("range", HeaderValue::from_static("bytes=0-31"));
        let range = read(&state, id, "download", headers).await;
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        let got = bytes(range).await;
        assert_eq!(&got[..16], b"\0\0\0\x20ftypisom\0\0\0\0");
        assert_eq!(&got[16..], &[0; 16]);
        assert_eq!(
            content(
                State(state),
                RoutePath((id.into(), "thumbnail".into())),
                crate::bearer_headers("token")
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }
}
