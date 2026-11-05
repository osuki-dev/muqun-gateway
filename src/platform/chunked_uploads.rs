//! Chunked uploads: a large video sent as many small ordinary requests.
//!
//! A sealed request is one AES-GCM envelope over its whole body, so it cannot
//! be streamed, and `POST /api/uploads` therefore buffers up to 64 MiB. Here
//! each request carries one chunk of at most [`CHUNK_BYTES`], appended to a
//! partial file on disk as it arrives, so memory per request is one chunk
//! however large the file is. The finished file goes through the same content
//! sniffing as the single-shot route and answers in the same shape.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::routing::{delete, post, put};
use axum::Router;
use http_body_util::BodyExt as _;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncSeekExt as _, AsyncWriteExt as _};

use super::uploads::{
    ensure_uploads_dir, sanitize_upload_name, sniff_upload_file, stored_upload_name, upload_url,
    write_upload_file, MAX_UPLOAD_BYTES,
};
use crate::{api_error, require_device, ApiResult, AppState};

/// Largest chunk one request may carry, and what the app is told to send. It
/// is also the route's plaintext body limit: a sealed chunk of this size is
/// about 14 MiB on the wire and is the most one request holds in memory.
pub(crate) const CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Ceiling on a video sent in chunks. Other kinds keep [`MAX_UPLOAD_BYTES`].
pub(crate) const MAX_CHUNKED_VIDEO_BYTES: u64 = 1024 * 1024 * 1024;
/// Unfinished uploads one device may have open at once.
pub(crate) const MAX_CHUNKED_PER_DEVICE: usize = 2;
/// An upload that has seen no chunk for this long is abandoned and swept.
const CHUNKED_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// A device at its limit may reclaim its own uploads idle this long (the
/// `idleTimeoutMs` discovery advertises): the app that opened them is gone.
const CHUNKED_RECLAIM_AFTER: Duration = Duration::from_secs(60);
const PARTIAL_DIR: &str = ".partial";
const CHUNKED_PATH: &str = "/api/uploads/chunked";

const VIDEO_EXTENSIONS: [&str; 11] = [
    "mp4", "m4v", "mov", "3gp", "3g2", "webm", "mkv", "avi", "mpg", "mpeg", "flv",
];

struct Chunked {
    device_id: String,
    name: String,
    size: u64,
    received: u64,
    next_index: u64,
    /// Length and digest of the last chunk taken, so a retry of it (the
    /// response was lost) can be told from a different chunk with its index.
    last: Option<(u64, [u8; 32])>,
    in_flight: bool,
    touched: Instant,
    /// The answer to `complete`, kept so a retry of it gets the same one.
    done: Option<Value>,
}

static REGISTRY: Mutex<Option<HashMap<String, Chunked>>> = Mutex::new(None);

fn registry() -> MutexGuard<'static, Option<HashMap<String, Chunked>>> {
    let mut guard = REGISTRY.lock().unwrap_or_else(|p| p.into_inner());
    guard.get_or_insert_with(HashMap::new);
    guard
}

fn with_registry<T>(f: impl FnOnce(&mut HashMap<String, Chunked>) -> T) -> T {
    f(registry().as_mut().expect("registry initialised"))
}

pub(crate) fn is_chunk_path(path: &str) -> bool {
    path.strip_prefix(CHUNKED_PATH)
        .and_then(|rest| rest.strip_prefix('/'))
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(id, index)| {
            !index.is_empty() && index != "complete" && !index.contains('/') && !id.is_empty()
        })
}

fn not_found() -> (StatusCode, Json<Value>) {
    super::uploads::upload_not_found()
}

fn failed() -> (StatusCode, Json<Value>) {
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "upload_failed",
        "failed to store the upload",
    )
}

fn partial_path(upload_id: &str) -> anyhow::Result<PathBuf> {
    Ok(super::uploads::uploads_dir()?
        .join(PARTIAL_DIR)
        .join(upload_id))
}

fn parse_id(raw: &str) -> Result<String, (StatusCode, Json<Value>)> {
    uuid::Uuid::parse_str(raw)
        .map(|id| id.to_string())
        .map_err(|_| not_found())
}

fn claims_video(name: &str, mime: Option<&str>) -> bool {
    mime.is_some_and(|mime| mime.trim().to_ascii_lowercase().starts_with("video/"))
        || name.rsplit_once('.').is_some_and(|(_, extension)| {
            VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn remove_partial(upload_id: &str) {
    if let Ok(path) = partial_path(upload_id) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(serde::Deserialize)]
struct CreateBody {
    name: String,
    size: u64,
    #[serde(default)]
    mime: Option<String>,
}

/// `POST /api/uploads/chunked`: open an upload and say how to send it.
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let request: CreateBody = serde_json::from_slice(&body).map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "expected a JSON body with name and size",
        )
    })?;
    if request.size == 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_file",
            "the file is empty",
        ));
    }
    let claimed_video = claims_video(&request.name, request.mime.as_deref());
    let maximum = if claimed_video {
        MAX_CHUNKED_VIDEO_BYTES
    } else {
        MAX_UPLOAD_BYTES as u64
    };
    if request.size > maximum {
        return Err(too_large(maximum));
    }

    let upload_id = uuid::Uuid::new_v4().to_string();
    let now = Instant::now();
    with_registry(|uploads| {
        uploads.retain(|_, upload| now.duration_since(upload.touched) < CHUNKED_IDLE_TTL);
        let mine: Vec<(String, Instant)> = uploads
            .iter()
            .filter(|(_, upload)| upload.device_id == device_id && upload.done.is_none())
            .map(|(id, upload)| (id.clone(), upload.touched))
            .collect();
        if mine.len() >= MAX_CHUNKED_PER_DEVICE {
            let stale = mine.iter().find(|(id, touched)| {
                now.duration_since(*touched) >= CHUNKED_RECLAIM_AFTER
                    && uploads.get(id).is_some_and(|upload| !upload.in_flight)
            });
            let Some((stale, _)) = stale else {
                return Err(api_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too_many_uploads",
                    "finish or abort an open upload first",
                ));
            };
            let stale = stale.clone();
            uploads.remove(&stale);
            remove_partial(&stale);
        }
        Ok(())
    })?;

    let dir = ensure_uploads_dir().map_err(|err| {
        tracing::warn!("failed to prepare the upload directory: {err:#}");
        failed()
    })?;
    let partial_dir = dir.join(PARTIAL_DIR);
    std::fs::create_dir_all(&partial_dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| write_upload_file(&partial_dir.join(&upload_id), &[]))
        .map_err(|err| {
            tracing::warn!("failed to open a chunked upload: {err:#}");
            failed()
        })?;
    with_registry(|uploads| {
        uploads.insert(
            upload_id.clone(),
            Chunked {
                device_id,
                name: request.name,
                size: request.size,
                received: 0,
                next_index: 0,
                last: None,
                in_flight: false,
                touched: now,
                done: None,
            },
        );
    });
    Ok(Json(
        json!({ "uploadId": upload_id, "chunkBytes": CHUNK_BYTES }),
    ))
}

fn too_large(maximum: u64) -> (StatusCode, Json<Value>) {
    let mib = maximum / (1024 * 1024);
    api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "upload_too_large",
        &format!("this file may be at most {mib} MiB"),
    )
}

/// Marks an upload busy while a chunk is written, and puts the partial file
/// back to the last whole chunk if the write is cut short -- a failure or a
/// phone that went away -- so the same chunk can be sent again.
struct ChunkGuard {
    upload_id: String,
    path: PathBuf,
    committed_len: u64,
    committed: bool,
}

impl Drop for ChunkGuard {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&self.path) {
                let _ = file.set_len(self.committed_len);
            }
        }
        with_registry(|uploads| {
            if let Some(upload) = uploads.get_mut(&self.upload_id) {
                upload.in_flight = false;
                upload.touched = Instant::now();
            }
        });
    }
}

/// `PUT /api/uploads/chunked/{uploadId}/{index}`: one chunk, raw bytes.
async fn put_chunk(
    State(state): State<AppState>,
    Path((raw_id, index)): Path<(String, u64)>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let upload_id = parse_id(&raw_id)?;

    enum Plan {
        Write { received: u64, size: u64 },
        Repeat(Value, u64, [u8; 32]),
    }
    let plan = with_registry(|uploads| {
        let upload = uploads
            .get_mut(&upload_id)
            .filter(|upload| upload.device_id == device_id)
            .ok_or_else(not_found)?;
        if upload.done.is_some() || upload.in_flight {
            return Err(api_error(
                StatusCode::CONFLICT,
                "upload_busy",
                "this upload is finished or already receiving a chunk",
            ));
        }
        if index + 1 == upload.next_index {
            let (len, digest) = upload.last.expect("a chunk was taken");
            return Ok(Plan::Repeat(chunk_answer(&upload_id, upload), len, digest));
        }
        if index != upload.next_index {
            return Err(api_error(
                StatusCode::CONFLICT,
                "chunk_out_of_order",
                &format!("expected chunk {}", upload.next_index),
            ));
        }
        upload.in_flight = true;
        Ok(Plan::Write {
            received: upload.received,
            size: upload.size,
        })
    })?;

    let (received, size) = match plan {
        Plan::Write { received, size } => (received, size),
        Plan::Repeat(answer, len, digest) => {
            // Same index, so it is the same chunk only if it is the same bytes.
            let (got_len, got_digest) = digest_of(body).await?;
            if got_len == len && got_digest == digest {
                return Ok(Json(answer));
            }
            return Err(api_error(
                StatusCode::CONFLICT,
                "chunk_out_of_order",
                "that chunk was already taken with different bytes",
            ));
        }
    };

    let path = partial_path(&upload_id).map_err(|_| failed())?;
    let mut guard = ChunkGuard {
        upload_id: upload_id.clone(),
        path: path.clone(),
        committed_len: received,
        committed: false,
    };
    let limit = (CHUNK_BYTES as u64).min(size - received);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .map_err(|err| {
            tracing::warn!("failed to open {}: {err}", path.display());
            failed()
        })?;
    file.seek(std::io::SeekFrom::Start(received))
        .await
        .map_err(|_| failed())?;
    let mut hasher = Sha256::new();
    let mut taken = 0u64;
    let mut body = body;
    while let Some(frame) = body
        .frame()
        .await
        .transpose()
        .map_err(super::server::body_read_error)?
    {
        let Ok(data) = frame.into_data() else {
            continue;
        };
        taken += data.len() as u64;
        if taken > limit {
            let (code, message) = if taken > CHUNK_BYTES as u64 {
                ("chunk_too_large", "a chunk may be at most 8 MiB")
            } else {
                ("upload_too_large", "more bytes than the upload declared")
            };
            return Err(api_error(StatusCode::PAYLOAD_TOO_LARGE, code, message));
        }
        hasher.update(&data);
        file.write_all(&data).await.map_err(|err| {
            tracing::warn!("failed to write a chunk: {err}");
            failed()
        })?;
    }
    if taken == 0 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_chunk",
            "the chunk is empty",
        ));
    }
    file.flush().await.map_err(|_| failed())?;
    drop(file);

    let answer = with_registry(|uploads| {
        let upload = uploads.get_mut(&upload_id).ok_or_else(not_found)?;
        upload.received += taken;
        upload.next_index += 1;
        upload.last = Some((taken, hasher.finalize().into()));
        Ok::<_, (StatusCode, Json<Value>)>(chunk_answer(&upload_id, upload))
    })?;
    guard.committed = true;
    Ok(Json(answer))
}

fn chunk_answer(upload_id: &str, upload: &Chunked) -> Value {
    json!({
        "uploadId": upload_id,
        "index": upload.next_index - 1,
        "received": upload.received,
        "size": upload.size,
    })
}

async fn digest_of(mut body: Body) -> ApiResult<(u64, [u8; 32])> {
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    while let Some(frame) = body
        .frame()
        .await
        .transpose()
        .map_err(super::server::body_read_error)?
    {
        if let Ok(data) = frame.into_data() {
            len += data.len() as u64;
            if len > CHUNK_BYTES as u64 {
                break;
            }
            hasher.update(&data);
        }
    }
    Ok((len, hasher.finalize().into()))
}

/// `POST /api/uploads/chunked/{uploadId}/complete`: sniff and publish.
async fn complete(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let upload_id = parse_id(&raw_id)?;

    enum Step {
        Repeat(Value),
        Finish { name: String, size: u64 },
    }
    let step = with_registry(|uploads| {
        let upload = uploads
            .get_mut(&upload_id)
            .filter(|upload| upload.device_id == device_id)
            .ok_or_else(not_found)?;
        if let Some(done) = &upload.done {
            return Ok(Step::Repeat(done.clone()));
        }
        if upload.in_flight {
            return Err(api_error(
                StatusCode::CONFLICT,
                "upload_busy",
                "this upload is still receiving a chunk",
            ));
        }
        if upload.received != upload.size {
            return Err(api_error(
                StatusCode::CONFLICT,
                "upload_incomplete",
                &format!("received {} of {} bytes", upload.received, upload.size),
            ));
        }
        upload.in_flight = true;
        Ok(Step::Finish {
            name: upload.name.clone(),
            size: upload.size,
        })
    })?;
    let (name, size) = match step {
        Step::Repeat(done) => return Ok(Json(done)),
        Step::Finish { name, size } => (name, size),
    };

    let partial = partial_path(&upload_id).map_err(|_| failed())?;
    let outcome = {
        let partial = partial.clone();
        let name = name.clone();
        tokio::task::spawn_blocking(move || judge(&partial, &name, size))
            .await
            .unwrap_or_else(|_| Err(failed()))
    };
    let kind = match outcome {
        Ok(kind) => kind,
        Err(refusal) => {
            // The file is not coming back as something else: drop it.
            with_registry(|uploads| uploads.remove(&upload_id));
            remove_partial(&upload_id);
            return Err(refusal);
        }
    };

    let stored_name = stored_upload_name(kind);
    let published = ensure_uploads_dir()
        .and_then(|dir| {
            let path = dir.join(&stored_name);
            std::fs::rename(&partial, &path)?;
            Ok(path)
        })
        .map_err(|err| {
            tracing::warn!("failed to publish a chunked upload: {err:#}");
            with_registry(|uploads| {
                if let Some(upload) = uploads.get_mut(&upload_id) {
                    upload.in_flight = false;
                }
            });
            failed()
        })?;
    let answer = json!({
        "path": published.to_string_lossy(),
        "url": upload_url(&stored_name),
        "name": sanitize_upload_name(&name),
        "size": size,
        "mime": kind.mime
    });
    with_registry(|uploads| {
        if let Some(upload) = uploads.get_mut(&upload_id) {
            upload.in_flight = false;
            upload.touched = Instant::now();
            upload.done = Some(answer.clone());
        }
    });
    Ok(Json(answer))
}

/// What the finished file is, by the single-shot route's own rules: not an
/// executable, a type the gateway stores, and within that type's ceiling.
fn judge(
    partial: &FsPath,
    name: &str,
    size: u64,
) -> Result<super::uploads::UploadKind, (StatusCode, Json<Value>)> {
    let head = crate::read_asset_head(partial);
    if crate::looks_executable(&head) {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "executable_rejected",
            "executables and scripts are not accepted",
        ));
    }
    // An Office package is probed whole; one too big to be a document is not
    // read at all.
    if size > MAX_UPLOAD_BYTES as u64 && head.starts_with(super::uploads::ZIP_LOCAL_HEADER) {
        return Err(too_large(MAX_UPLOAD_BYTES as u64));
    }
    let Some(kind) = sniff_upload_file(partial, name) else {
        return Err(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_file_type",
            "only supported images, videos, office documents, PDF, and UTF-8 text are accepted",
        ));
    };
    let maximum = if kind.mime.starts_with("video/") {
        MAX_CHUNKED_VIDEO_BYTES
    } else {
        MAX_UPLOAD_BYTES as u64
    };
    if size > maximum {
        return Err(too_large(maximum));
    }
    Ok(kind)
}

/// `DELETE /api/uploads/chunked/{uploadId}`: give up on an upload. After
/// `complete` it only forgets the upload; the stored file is the app's now.
async fn abort(
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let device_id = require_device(&state, &headers)?;
    let upload_id = parse_id(&raw_id)?;
    let removed = with_registry(|uploads| {
        if uploads
            .get(&upload_id)
            .is_some_and(|upload| upload.device_id == device_id)
        {
            uploads.remove(&upload_id)
        } else {
            None
        }
    })
    .ok_or_else(not_found)?;
    if removed.done.is_none() {
        remove_partial(&upload_id);
    }
    Ok(Json(json!({ "ok": true })))
}

/// Forget uploads nobody has touched for [`CHUNKED_IDLE_TTL`] and delete their
/// partial files, and any partial file no upload owns. At startup nothing is
/// owned: the registry lives in memory, so whatever a previous run left is
/// abandoned however young.
pub(crate) fn sweep(uploads_dir: &FsPath, startup: bool) {
    let now = Instant::now();
    let owned: Vec<String> = with_registry(|uploads| {
        let expired: Vec<String> = uploads
            .iter()
            .filter(|(_, upload)| now.duration_since(upload.touched) >= CHUNKED_IDLE_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            if uploads
                .remove(&id)
                .is_some_and(|upload| upload.done.is_none())
            {
                remove_partial(&id);
            }
        }
        uploads.keys().cloned().collect()
    });
    let Ok(entries) = std::fs::read_dir(uploads_dir.join(PARTIAL_DIR)) else {
        return;
    };
    let wall = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if owned.contains(&name) {
            continue;
        }
        let idle = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| wall.duration_since(modified).ok());
        if startup || idle.is_some_and(|idle| idle >= CHUNKED_IDLE_TTL) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The four chunked routes. `uploads::mount` calls this, so they are mounted
/// wherever uploads are.
pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route(CHUNKED_PATH, post(create))
        .route(&format!("{CHUNKED_PATH}/{{upload_id}}"), delete(abort))
        .route(
            &format!("{CHUNKED_PATH}/{{upload_id}}/complete"),
            post(complete),
        )
        .route(
            &format!("{CHUNKED_PATH}/{{upload_id}}/{{index}}"),
            put(put_chunk).layer(DefaultBodyLimit::max(CHUNK_BYTES)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use tower::ServiceExt as _;

    /// Uploads are counted per device and the registry is process-wide, so
    /// each test pairs its own phones.
    fn app(tokens: &[&str]) -> Router {
        let state = test_state(
            "admin",
            tokens
                .iter()
                .map(|token| test_device(token, token))
                .collect(),
        );
        super::mount(Router::new()).with_state(state)
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        token: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn open(app: &Router, token: &str, name: &str, size: u64) -> (StatusCode, Value) {
        let body = json!({ "name": name, "size": size })
            .to_string()
            .into_bytes();
        send(app, "POST", CHUNKED_PATH, token, body).await
    }

    fn put_uri(id: &str, index: u64) -> String {
        format!("{CHUNKED_PATH}/{id}/{index}")
    }

    fn mp4(len: usize) -> Vec<u8> {
        let mut bytes = b"\x00\x00\x00\x14ftypisom\x00\x00\x00\x00isom".to_vec();
        bytes.extend((0..len - bytes.len()).map(|i| (i % 251) as u8));
        bytes
    }

    #[tokio::test]
    async fn a_video_sent_in_three_chunks_is_stored_whole_and_answers_like_the_single_shot_route() {
        let (a, b) = ("t1a", "t1b");
        let app = app(&[a, b]);
        let video = mp4(30_000);
        let (status, opened) = open(&app, a, "clip.MP4", video.len() as u64).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(opened["chunkBytes"], CHUNK_BYTES);
        let id = opened["uploadId"].as_str().unwrap();

        let parts = [&video[..12_000], &video[12_000..25_000], &video[25_000..]];
        for (index, part) in parts.iter().enumerate() {
            let (status, answer) =
                send(&app, "PUT", &put_uri(id, index as u64), a, part.to_vec()).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                answer["received"],
                parts[..=index].iter().map(|p| p.len()).sum::<usize>()
            );
        }
        // The answer to the last chunk was lost: sending it again is fine,
        // and sending other bytes under that index is not.
        let (status, _) = send(&app, "PUT", &put_uri(id, 2), a, parts[2].to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(&app, "PUT", &put_uri(id, 2), a, vec![1; parts[2].len()]).await;
        assert_eq!(status, StatusCode::CONFLICT);

        let complete_uri = format!("{CHUNKED_PATH}/{id}/complete");
        let (status, stored) = send(&app, "POST", &complete_uri, a, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        let mut keys: Vec<_> = stored.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["mime", "name", "path", "size", "url"]);
        assert_eq!(stored["mime"], "video/mp4");
        assert_eq!(stored["name"], "clip.MP4");
        assert_eq!(stored["size"], video.len());
        assert_eq!(
            std::fs::read(stored["path"].as_str().unwrap()).unwrap(),
            video
        );
        assert!(stored["url"].as_str().unwrap().starts_with("/api/uploads/"));
        // Asking again returns the same answer instead of a new file.
        let (_, again) = send(&app, "POST", &complete_uri, a, vec![]).await;
        assert_eq!(again, stored);
        std::fs::remove_file(stored["path"].as_str().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn chunks_out_of_order_over_the_declared_size_or_of_an_unsupported_type_are_refused() {
        let (a, b) = ("t2a", "t2b");
        let app = app(&[a, b]);
        let video = mp4(1_000);

        // Refused when the upload is opened: past the ceiling of its kind.
        for (name, size) in [
            ("big.mp4", MAX_CHUNKED_VIDEO_BYTES + 1),
            ("photo.png", MAX_UPLOAD_BYTES as u64 + 1),
            ("empty.mp4", 0),
        ] {
            let (status, _) = open(&app, a, name, size).await;
            assert_ne!(status, StatusCode::OK, "{name}");
        }
        let (status, ok) = open(&app, a, "big.mp4", MAX_CHUNKED_VIDEO_BYTES).await;
        assert_eq!(status, StatusCode::OK);
        send(
            &app,
            "DELETE",
            &format!("{CHUNKED_PATH}/{}", ok["uploadId"].as_str().unwrap()),
            a,
            vec![],
        )
        .await;

        // (what to send first, what happens)
        let (_, opened) = open(&app, a, "clip.mp4", video.len() as u64).await;
        let id = opened["uploadId"].as_str().unwrap().to_string();
        let (status, refused) = send(&app, "PUT", &put_uri(&id, 1), a, video.clone()).await;
        assert_eq!(
            (status, &refused["error"]["code"]),
            (StatusCode::CONFLICT, &json!("chunk_out_of_order"))
        );
        let (status, refused) = send(&app, "PUT", &put_uri(&id, 0), a, mp4(1_001)).await;
        assert_eq!(
            (status, &refused["error"]["code"]),
            (StatusCode::PAYLOAD_TOO_LARGE, &json!("upload_too_large"))
        );
        // The refused chunk left nothing behind: the whole one still fits.
        let (status, _) = send(&app, "PUT", &put_uri(&id, 0), a, video.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let complete_uri = format!("{CHUNKED_PATH}/{id}/complete");
        let (status, _) = send(&app, "POST", &complete_uri, a, vec![]).await;
        assert_eq!(status, StatusCode::OK);

        // Not complete yet.
        let (_, opened) = open(&app, a, "clip.mp4", 2_000).await;
        let id = opened["uploadId"].as_str().unwrap().to_string();
        send(&app, "PUT", &put_uri(&id, 0), a, video.clone()).await;
        let (status, _) = send(
            &app,
            "POST",
            &format!("{CHUNKED_PATH}/{id}/complete"),
            a,
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        send(&app, "DELETE", &format!("{CHUNKED_PATH}/{id}"), a, vec![]).await;

        // Called a video, is not: refused at complete, and the partial is gone.
        let (_, opened) = open(&app, a, "clip.mp4", 64).await;
        let id = opened["uploadId"].as_str().unwrap().to_string();
        send(&app, "PUT", &put_uri(&id, 0), a, vec![0x01; 64]).await;
        let partial = partial_path(&id).unwrap();
        assert!(partial.exists());
        let (status, refused) = send(
            &app,
            "POST",
            &format!("{CHUNKED_PATH}/{id}/complete"),
            a,
            vec![],
        )
        .await;
        assert_eq!(
            (status, &refused["error"]["code"]),
            (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                &json!("unsupported_file_type")
            )
        );
        assert!(!partial.exists());
        let (status, _) = send(
            &app,
            "POST",
            &format!("{CHUNKED_PATH}/{id}/complete"),
            a,
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_upload_belongs_to_its_device_and_aborting_it_removes_the_partial() {
        let (a, b) = ("t3a", "t3b");
        let app = app(&[a, b]);
        let (_, opened) = open(&app, a, "clip.mp4", 1_000).await;
        let id = opened["uploadId"].as_str().unwrap().to_string();
        let (status, _) = send(&app, "PUT", &put_uri(&id, 0), a, mp4(500)).await;
        assert_eq!(status, StatusCode::OK);

        for (method, uri) in [
            ("PUT", put_uri(&id, 1)),
            ("POST", format!("{CHUNKED_PATH}/{id}/complete")),
            ("DELETE", format!("{CHUNKED_PATH}/{id}")),
        ] {
            let (status, _) = send(&app, method, &uri, b, mp4(500)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        }
        let partial = partial_path(&id).unwrap();
        assert_eq!(std::fs::metadata(&partial).unwrap().len(), 500);

        let (status, _) = send(&app, "DELETE", &format!("{CHUNKED_PATH}/{id}"), a, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!partial.exists());
        let (status, _) = send(&app, "PUT", &put_uri(&id, 1), a, mp4(500)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_device_may_hold_only_so_many_open_uploads_and_abandoned_partials_are_swept() {
        let (a, b) = ("t4a", "t4b");
        let app = app(&[a, b]);
        let mut ids = Vec::new();
        for _ in 0..MAX_CHUNKED_PER_DEVICE {
            let (status, opened) = open(&app, b, "clip.mp4", 100).await;
            assert_eq!(status, StatusCode::OK);
            ids.push(opened["uploadId"].as_str().unwrap().to_string());
        }
        let (status, _) = open(&app, b, "clip.mp4", 100).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        // a leftover from a previous run is nobody's; a live one is kept.
        let orphan = partial_path(&uuid::Uuid::new_v4().to_string()).unwrap();
        std::fs::write(&orphan, b"x").unwrap();
        sweep(&super::super::uploads::uploads_dir().unwrap(), false);
        assert!(orphan.exists(), "young orphans wait out the idle time");
        std::fs::File::options()
            .write(true)
            .open(&orphan)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - CHUNKED_IDLE_TTL - Duration::from_secs(60))
            .unwrap();
        sweep(&super::super::uploads::uploads_dir().unwrap(), false);
        assert!(!orphan.exists());
        for id in ids {
            send(&app, "DELETE", &format!("{CHUNKED_PATH}/{id}"), b, vec![]).await;
        }
    }

    #[test]
    fn only_a_chunk_put_takes_the_chunk_body_limit() {
        for (path, chunk) in [
            (
                "/api/uploads/chunked/6f1c0b1e-0000-0000-0000-000000000000/0",
                true,
            ),
            (
                "/api/uploads/chunked/6f1c0b1e-0000-0000-0000-000000000000/complete",
                false,
            ),
            (
                "/api/uploads/chunked/6f1c0b1e-0000-0000-0000-000000000000",
                false,
            ),
            ("/api/uploads/chunked", false),
            ("/api/uploads/6f1c.png", false),
        ] {
            assert_eq!(is_chunk_path(path), chunk, "{path}");
        }
    }
}
