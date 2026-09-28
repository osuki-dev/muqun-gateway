//! Upload ingestion for the phone's attachments.

use std::path::{Path as FsPath, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Json, Response};
use serde_json::{json, Value};

use crate::{
    api_error, looks_textual, read_asset_head, require_device, state_dir, ApiResult, AppState,
    ASSET_CONTENT_CHUNK_BYTES,
};

/// The one route allowed a body bigger than [`MAX_REQUEST_BODY_BYTES`]. Named
/// so the router and the encrypted transport cannot disagree about which route
/// that is.
pub(crate) const UPLOADS_PATH: &str = "/api/uploads";
pub(crate) const UPLOADS_DIR: &str = "uploads";
/// Ceiling on one upload body, enforced by the framework's body limit so an
/// oversized request is cut off mid-stream instead of being buffered first.
pub(crate) const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;
/// The client's own file name is only ever echoed back, never used to build a
/// path, so this is a display cap rather than a safety one.
pub(crate) const MAX_UPLOAD_NAME_CHARS: usize = 120;
/// Uploads are a handoff to a local agent, not storage: whatever the agent was
/// going to do with a file, it has done long before this.
pub(crate) const UPLOAD_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);
pub(crate) const UPLOAD_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// A file type the gateway is willing to store. Binary formats get both fields
/// from their magic bytes; UTF-8 text earns only a small extension allow-list
/// after its content has passed the text probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UploadKind {
    pub(crate) extension: &'static str,
    pub(crate) mime: &'static str,
}

/// Accept a file from the phone, park it in the gateway's own upload
/// directory and hand back a local path. The app then sends that path to an
/// agent as ordinary text, so the agent reads the file straight off this
/// machine.
///
/// Images and PDFs are identified by magic bytes. Source and document text has
/// to be valid, low-control UTF-8 before its display-name extension is allowed
/// to influence the generated stored name. Nothing from the client is ever
/// used as a path.
pub(crate) async fn upload_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    // Taking the rejection by hand keeps authorization ahead of body parsing,
    // and keeps every failure in the same JSON error shape as the other routes.
    require_device(&state, &headers)?;
    let mut multipart = multipart.map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_multipart",
            "expected a multipart/form-data body with a file field",
        )
    })?;

    let mut upload = None;
    while let Some(field) = multipart.next_field().await.map_err(upload_body_error)? {
        if field.name() != Some("file") {
            continue;
        }
        let client_name = field.file_name().map(str::to_owned);
        let bytes = field.bytes().await.map_err(upload_body_error)?;
        upload = Some((client_name, bytes));
        break;
    }

    let Some((client_name, bytes)) = upload else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_file",
            "expected a multipart/form-data body with a file field",
        ));
    };
    let Some(client_name) = client_name else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "missing_filename",
            "the file field must carry a filename",
        ));
    };
    if bytes.is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_file",
            "the file field is empty",
        ));
    }

    // Executables are checked separately from the content allow-list so the
    // refusal says what it means, and so adding a document format cannot
    // accidentally make an executable acceptable.
    if looks_executable(&bytes) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "executable_rejected",
            "executables and scripts are not accepted",
        ));
    }
    let Some(kind) = sniff_upload_kind(&bytes)
        .or_else(|| sniff_office_upload_kind(&bytes))
        .or_else(|| sniff_document_upload_kind(&bytes, &client_name))
    else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_file_type",
            "only supported images, office documents, PDF, and UTF-8 text are accepted",
        ));
    };

    let dir = ensure_uploads_dir().map_err(|err| {
        eprintln!("failed to prepare the upload directory: {err:#}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;
    let stored_name = stored_upload_name(kind);
    let path = dir.join(&stored_name);
    write_upload_file(&path, &bytes).map_err(|err| {
        eprintln!("failed to write upload {}: {err:#}", path.display());
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "upload_failed",
            "failed to store the upload",
        )
    })?;

    Ok(Json(json!({
        // `path` is for the agent, which reads the file off this host.
        "path": path.to_string_lossy(),
        // `url` is for the app, which cannot. Same file, the two ways of
        // reaching it, so the transcript can draw what was sent.
        "url": upload_url(&stored_name),
        "name": sanitize_upload_name(&client_name),
        "size": bytes.len(),
        "mime": kind.mime
    })))
}

/// Where a stored upload is readable over the API. The stored name is
/// generated by [`stored_upload_name`] from a UUID and an earned extension, so
/// it is already URL-safe and needs no escaping.
pub(crate) fn upload_url(stored_name: &str) -> String {
    format!("{UPLOADS_PATH}/{stored_name}")
}

/// Stream one stored upload back to the app, read-only.
///
/// The app holds the host path of its own attachment and cannot open it, so
/// this is the same bytes under a URL. Nothing here trusts the name: it has to
/// be a single path component of the alphabet the gateway itself generates,
/// the file has to be a regular file directly inside the upload directory, and
/// a file the sweeper would already have taken is a miss rather than a read,
/// so the 48-hour retention holds whether or not the hourly sweep has run.
pub(crate) async fn upload_content(
    State(state): State<AppState>,
    Path(file_name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_device(&state, &headers)?;

    let Some(name) = safe_upload_component(&file_name) else {
        return Err(upload_not_found());
    };
    let dir = uploads_dir().map_err(|err| {
        eprintln!("failed to resolve the upload directory: {err:#}");
        upload_not_found()
    })?;
    let path = dir.join(&name);

    // `symlink_metadata` rather than `metadata`: a symlink parked in the
    // upload directory must not become a way to read the rest of the host.
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| upload_not_found())?;
    if !metadata.is_file() {
        return Err(upload_not_found());
    }
    let modified = metadata.modified().map_err(|_| upload_not_found())?;
    if upload_expired(modified, SystemTime::now()) {
        return Err(upload_not_found());
    }

    let sniff_path = path.clone();
    let sniff_name = name.clone();
    let kind = tokio::task::spawn_blocking(move || sniff_stored_upload(&sniff_path, &sniff_name))
        .await
        .unwrap_or_default();
    let Some(kind) = kind else {
        return Err(upload_not_found());
    };

    let file = tokio::fs::File::open(&path).await.map_err(|err| {
        eprintln!("failed to open upload {}: {err}", path.display());
        upload_not_found()
    })?;
    let stream = async_stream::stream! {
        let mut file = file;
        let mut buffer = vec![0u8; ASSET_CONTENT_CHUNK_BYTES];
        loop {
            match tokio::io::AsyncReadExt::read(&mut file, &mut buffer).await {
                Ok(0) => break,
                Ok(read) => yield Ok::<_, std::io::Error>(
                    axum::body::Bytes::copy_from_slice(&buffer[..read]),
                ),
                Err(err) => {
                    yield Err(err);
                    break;
                }
            }
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", kind.mime)
        .header("content-length", metadata.len())
        .header(
            "content-disposition",
            format!("inline; filename=\"{name}\""),
        )
        // An upload is one device's own file, never a shared one: `private`
        // on top of the blanket `no-store` the security headers apply, so it
        // cannot land in a shared cache on the way back either.
        .header("cache-control", "private, no-store, max-age=0")
        .body(Body::from_stream(stream))
        .map_err(|err| {
            eprintln!("failed to build upload response: {err}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "upload_read_failed",
                "failed to read the upload",
            )
        })
}

/// One answer for every way of missing: an unknown name, a traversal, a
/// symlink, a swept file, and content this gateway would not have stored are
/// indistinguishable, so a caller cannot map the host by asking.
pub(crate) fn upload_not_found() -> (StatusCode, Json<Value>) {
    api_error(StatusCode::NOT_FOUND, "upload_not_found", "no such upload")
}

/// Reduce a path parameter to a name that can only ever mean one file inside
/// the upload directory, or to nothing.
///
/// The gateway generates every stored name itself -- a UUID, a dot, and a
/// lowercase extension -- so the accepted alphabet is exactly that and the
/// answer is a whole-name decision, not an escaping one. `..`, a separator, a
/// NUL, a percent-decoded separator and a leading dot all fail the same way.
pub(crate) fn safe_upload_component(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_UPLOAD_NAME_CHARS {
        return None;
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return None;
    }
    if raw.starts_with('.') || raw.contains("..") {
        return None;
    }
    // Belt and braces: whatever the alphabet allowed, the name still has to be
    // one plain component as the platform reads it.
    let path = FsPath::new(raw);
    if path.components().count() != 1 || path.file_name()? != raw {
        return None;
    }
    Some(raw.to_string())
}

/// Decide a stored upload's type from its bytes, the same order `upload_file`
/// used to accept it: magic numbers first, the office container probe for a
/// zip, and only then the text probe, where the stored extension -- which the
/// gateway earned from the content at upload time, never took from the client
/// -- picks the flavour.
pub(crate) fn sniff_stored_upload(path: &FsPath, stored_name: &str) -> Option<UploadKind> {
    let head = read_asset_head(path);
    if head.is_empty() {
        return None;
    }
    if looks_executable(&head) {
        return None;
    }
    if let Some(kind) = sniff_upload_kind(&head) {
        return Some(kind);
    }
    // A zip's central directory sits at the tail, so the office probe is the
    // one that needs the whole file -- and only when the head says zip.
    if head.starts_with(b"PK\x03\x04") {
        let whole = std::fs::read(path).ok()?;
        if let Some(kind) = sniff_office_upload_kind(&whole) {
            return Some(kind);
        }
    }
    sniff_document_upload_kind(&head, stored_name)
}

/// The body limit is enforced by the framework while the body streams, so an
/// oversized upload surfaces here as a length-limit error rather than as a
/// fully buffered file the gateway then has to measure.
pub(crate) fn upload_body_error(err: MultipartError) -> (StatusCode, Json<Value>) {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "upload_too_large",
            "the upload must be at most 25 MiB",
        );
    }
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_multipart",
        "expected a multipart/form-data body with a file field",
    )
}

/// Reject anything the host could be talked into running, whatever the file is
/// called. Extensions are not consulted: only the leading bytes are.
pub(crate) fn looks_executable(bytes: &[u8]) -> bool {
    /// Mach-O thin and fat binaries, in both byte orders. `cafebabe` also
    /// covers Java class files, which is no loss here.
    const MACH_O_MAGICS: [[u8; 4]; 6] = [
        [0xfe, 0xed, 0xfa, 0xce],
        [0xfe, 0xed, 0xfa, 0xcf],
        [0xce, 0xfa, 0xed, 0xfe],
        [0xcf, 0xfa, 0xed, 0xfe],
        [0xca, 0xfe, 0xba, 0xbe],
        [0xbe, 0xba, 0xfe, 0xca],
    ];

    bytes.starts_with(b"#!")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || MACH_O_MAGICS
            .iter()
            .any(|magic| bytes.starts_with(magic.as_slice()))
}

/// Decide what a file is from its content. Only images are accepted, and the
/// client's extension is never consulted, so a `.png` holding anything else is
/// refused however it was named.
pub(crate) fn sniff_upload_kind(bytes: &[u8]) -> Option<UploadKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(UploadKind {
            extension: "png",
            mime: "image/png",
        });
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(UploadKind {
            extension: "jpg",
            mime: "image/jpeg",
        });
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(UploadKind {
            extension: "gif",
            mime: "image/gif",
        });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(UploadKind {
            extension: "webp",
            mime: "image/webp",
        });
    }
    if is_heic(bytes) {
        return Some(UploadKind {
            extension: "heic",
            mime: "image/heic",
        });
    }
    None
}

/// Recognise modern Office/OpenDocument containers without extracting them.
/// The central directory carries entry names in plain bytes even when the
/// entries themselves are compressed, which is enough to distinguish a Word,
/// Excel or PowerPoint package from an arbitrary zip. ODF additionally puts an
/// uncompressed `mimetype` entry first by specification.
pub(crate) fn sniff_office_upload_kind(bytes: &[u8]) -> Option<UploadKind> {
    let names = zip_entry_names(bytes)?;
    let has = |needle: &str| names.contains(&needle);
    let ooxml_root = has("[Content_Types].xml") && has("_rels/.rels");
    let mut ooxml = Vec::new();
    if ooxml_root && has("word/document.xml") {
        ooxml.push(UploadKind {
            extension: "docx",
            mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        });
    }
    if ooxml_root && has("xl/workbook.xml") {
        ooxml.push(UploadKind {
            extension: "xlsx",
            mime: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        });
    }
    if ooxml_root && has("ppt/presentation.xml") {
        ooxml.push(UploadKind {
            extension: "pptx",
            mime: "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        });
    }
    if ooxml.len() == 1 {
        return ooxml.pop();
    }
    if !ooxml.is_empty() {
        // A package claiming to be multiple Office document kinds is not one
        // the app or an agent should be asked to interpret.
        return None;
    }

    if !has("content.xml") || !has("META-INF/manifest.xml") {
        return None;
    }
    let (name, mime) = stored_first_zip_entry(bytes)?;
    if name != "mimetype" {
        return None;
    }
    match mime {
        b"application/vnd.oasis.opendocument.text" => Some(UploadKind {
            extension: "odt",
            mime: "application/vnd.oasis.opendocument.text",
        }),
        b"application/vnd.oasis.opendocument.spreadsheet" => Some(UploadKind {
            extension: "ods",
            mime: "application/vnd.oasis.opendocument.spreadsheet",
        }),
        b"application/vnd.oasis.opendocument.presentation" => Some(UploadKind {
            extension: "odp",
            mime: "application/vnd.oasis.opendocument.presentation",
        }),
        _ => None,
    }
}

pub(crate) const ZIP_LOCAL_HEADER: &[u8; 4] = b"PK\x03\x04";

pub(crate) const ZIP_CENTRAL_HEADER: &[u8; 4] = b"PK\x01\x02";

pub(crate) const ZIP_END_HEADER: &[u8; 4] = b"PK\x05\x06";

pub(crate) const MAX_OFFICE_ZIP_ENTRIES: usize = 4_096;

pub(crate) fn zip_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

pub(crate) fn zip_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Read only bounded metadata from an ordinary (non-Zip64) central directory.
/// Encrypted entries, split archives and malformed offsets all fail closed.
pub(crate) fn zip_entry_names(bytes: &[u8]) -> Option<Vec<&str>> {
    if bytes.len() < 22 || !bytes.starts_with(ZIP_LOCAL_HEADER) {
        return None;
    }
    let search_start = bytes.len().saturating_sub(22 + u16::MAX as usize);
    let eocd = (search_start..=bytes.len() - 22)
        .rev()
        .find(|offset| bytes.get(*offset..*offset + 4) == Some(ZIP_END_HEADER.as_slice()))?;
    if zip_u16(bytes, eocd + 4)? != 0 || zip_u16(bytes, eocd + 6)? != 0 {
        return None;
    }
    let disk_entries = zip_u16(bytes, eocd + 8)? as usize;
    let entry_count = zip_u16(bytes, eocd + 10)? as usize;
    if entry_count != disk_entries || entry_count == 0 || entry_count > MAX_OFFICE_ZIP_ENTRIES {
        return None;
    }
    let central_size = zip_u32(bytes, eocd + 12)? as usize;
    let central_offset = zip_u32(bytes, eocd + 16)? as usize;
    let central_end = central_offset.checked_add(central_size)?;
    if central_end > eocd || central_end > bytes.len() {
        return None;
    }

    let mut cursor = central_offset;
    let mut names = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        if bytes.get(cursor..cursor + 4)? != ZIP_CENTRAL_HEADER {
            return None;
        }
        // Bit zero is traditional ZIP encryption. An agent could not inspect
        // it anyway, and accepting opaque encrypted containers would defeat
        // the entry-name validation this function exists to provide.
        if zip_u16(bytes, cursor + 8)? & 1 != 0 {
            return None;
        }
        let name_len = zip_u16(bytes, cursor + 28)? as usize;
        let extra_len = zip_u16(bytes, cursor + 30)? as usize;
        let comment_len = zip_u16(bytes, cursor + 32)? as usize;
        let name_start = cursor.checked_add(46)?;
        let name_end = name_start.checked_add(name_len)?;
        let next = name_end.checked_add(extra_len)?.checked_add(comment_len)?;
        if next > central_end {
            return None;
        }
        let name = std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?;
        if name.contains('\0') || name.starts_with('/') || name.split('/').any(|part| part == "..")
        {
            return None;
        }
        names.push(name);
        cursor = next;
    }
    (cursor == central_end).then_some(names)
}

/// ODF's first entry is mandated to be uncompressed `mimetype`. Reading that
/// tiny stored value does not inflate attacker-controlled data.
pub(crate) fn stored_first_zip_entry(bytes: &[u8]) -> Option<(&str, &[u8])> {
    if bytes.get(0..4)? != ZIP_LOCAL_HEADER {
        return None;
    }
    let flags = zip_u16(bytes, 6)?;
    let method = zip_u16(bytes, 8)?;
    if flags & 1 != 0 || method != 0 {
        return None;
    }
    let compressed = zip_u32(bytes, 18)? as usize;
    let uncompressed = zip_u32(bytes, 22)? as usize;
    if compressed != uncompressed || compressed > 256 {
        return None;
    }
    let name_len = zip_u16(bytes, 26)? as usize;
    let extra_len = zip_u16(bytes, 28)? as usize;
    let name_start = 30usize;
    let name_end = name_start.checked_add(name_len)?;
    let data_start = name_end.checked_add(extra_len)?;
    let data_end = data_start.checked_add(compressed)?;
    Some((
        std::str::from_utf8(bytes.get(name_start..name_end)?).ok()?,
        bytes.get(data_start..data_end)?,
    ))
}

/// Documents stay deliberately narrow: PDF has an unambiguous signature and
/// everything else must first pass the same UTF-8 text probe used by the asset
/// browser. The filename may then preserve a useful source/document extension,
/// but can never turn binary bytes into an accepted upload.
pub(crate) fn sniff_document_upload_kind(bytes: &[u8], client_name: &str) -> Option<UploadKind> {
    if bytes.starts_with(b"%PDF-") {
        return Some(UploadKind {
            extension: "pdf",
            mime: "application/pdf",
        });
    }
    if !looks_textual(bytes) {
        return None;
    }

    let extension = client_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    Some(match extension.as_deref() {
        Some("md" | "markdown" | "mdx") => UploadKind {
            extension: "md",
            mime: "text/markdown; charset=utf-8",
        },
        Some("json") => UploadKind {
            extension: "json",
            mime: "application/json",
        },
        Some("jsonl" | "ndjson") => UploadKind {
            extension: "jsonl",
            mime: "application/x-ndjson",
        },
        Some("csv") => UploadKind {
            extension: "csv",
            mime: "text/csv; charset=utf-8",
        },
        Some("yaml" | "yml") => UploadKind {
            extension: "yaml",
            mime: "application/yaml",
        },
        Some("toml") => UploadKind {
            extension: "toml",
            mime: "application/toml",
        },
        Some("xml") => UploadKind {
            extension: "xml",
            mime: "application/xml",
        },
        Some("rtf") => UploadKind {
            extension: "rtf",
            mime: "application/rtf",
        },
        Some("ts") => text_upload_kind("ts"),
        Some("tsx") => text_upload_kind("tsx"),
        Some("js") => text_upload_kind("js"),
        Some("jsx") => text_upload_kind("jsx"),
        Some("css") => text_upload_kind("css"),
        Some("html") => text_upload_kind("html"),
        Some("py") => text_upload_kind("py"),
        Some("rs") => text_upload_kind("rs"),
        Some("go") => text_upload_kind("go"),
        Some("java") => text_upload_kind("java"),
        Some("kt") => text_upload_kind("kt"),
        Some("swift") => text_upload_kind("swift"),
        Some("c") => text_upload_kind("c"),
        Some("h") => text_upload_kind("h"),
        Some("cpp") => text_upload_kind("cpp"),
        Some("hpp") => text_upload_kind("hpp"),
        Some("sql") => text_upload_kind("sql"),
        Some("log") => text_upload_kind("log"),
        _ => text_upload_kind("txt"),
    })
}

pub(crate) fn text_upload_kind(extension: &'static str) -> UploadKind {
    UploadKind {
        extension,
        mime: "text/plain; charset=utf-8",
    }
}

/// HEIC is an ISO base media file: a `ftyp` box whose brand names the flavour.
pub(crate) fn is_heic(bytes: &[u8]) -> bool {
    const HEIC_BRANDS: [&[u8; 4]; 10] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
    ];

    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && HEIC_BRANDS.iter().any(|brand| &bytes[8..12] == *brand)
}

/// The name on disk is generated here in full: a random stem plus the
/// extension the content earned. A client name can therefore never traverse,
/// collide, or smuggle in a second extension.
pub(crate) fn stored_upload_name(kind: UploadKind) -> String {
    format!("{}.{}", uuid::Uuid::new_v4(), kind.extension)
}

/// Reduce the client's file name to something safe to show. It is echoed back
/// so the app can label the attachment; it never touches the filesystem.
pub(crate) fn sanitize_upload_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .replace(|c: char| c.is_control(), "");
    let base = base.trim();
    if base.is_empty() || base.chars().all(|c| c == '.') {
        return String::from("upload");
    }
    base.chars().take(MAX_UPLOAD_NAME_CHARS).collect()
}

pub(crate) fn uploads_dir() -> anyhow::Result<PathBuf> {
    Ok(state_dir()?.join(UPLOADS_DIR))
}

/// Uploads live beside the gateway's other state, in their own directory so a
/// sweep can never reach a token file.
pub(crate) fn ensure_uploads_dir() -> anyhow::Result<PathBuf> {
    let dir = uploads_dir()?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create upload dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to lock down upload dir {}", dir.display()))?;
    }
    Ok(dir)
}

pub(crate) fn write_upload_file(path: &FsPath, bytes: &[u8]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        std::io::Write::write_all(&mut file, bytes)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Sweep old uploads at startup and every hour after that. A phone that
/// uploads a screenshot and loses interest should not leave it on the host.
pub(crate) fn spawn_upload_gc() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(UPLOAD_GC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // The first tick completes immediately, which is the startup sweep.
            ticker.tick().await;
            let Ok(dir) = uploads_dir() else {
                continue;
            };
            if let Err(err) = purge_expired_uploads(&dir, SystemTime::now()) {
                eprintln!("failed to sweep old uploads: {err:#}");
            }
        }
    });
}

/// Delete every stored upload older than the retention window. Returns how many
/// files were removed.
pub(crate) fn purge_expired_uploads(dir: &FsPath, now: SystemTime) -> anyhow::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("failed to read upload dir {}", dir.display()))?
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if !upload_expired(modified, now) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(err) => eprintln!("failed to remove {}: {err}", entry.path().display()),
        }
    }
    Ok(removed)
}

/// A file whose timestamp sits in the future is left alone rather than deleted:
/// a clock change should not destroy an upload the user just made.
pub(crate) fn upload_expired(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age >= UPLOAD_RETENTION)
        .unwrap_or(false)
}
