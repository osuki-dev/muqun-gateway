//! Explicit file handoff, never path scraping. Copies a bounded regular file
//! into the existing private, expiring upload store; the App must independently
//! be paired with the source computer to read it. No bearer URL is returned.
use crate::platform::uploads;
use crate::{api_error, ApiResult, AppState};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Seek};
use std::path::{Component, Path};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(crate) const CAPABILITY: &str = "mcp_file_present_v1";
static EXPORTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

struct PendingExport(Arc<AtomicBool>);
impl Drop for PendingExport {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route("/api/mcp/files/present", post(present_route))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    path: String,
}
async fn present_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Request>,
) -> ApiResult<Json<Value>> {
    let caller = crate::platform::http::require_device_or_peer(&state, &headers)?;
    if caller.kind == crate::connectivity::authority::DeviceKind::Gateway
        && !state.peers.accepts_delegations()
    {
        return Err(crate::platform::http::peer_forbidden());
    }
    present(&state, body.path)
        .await
        .map(Json)
        .map_err(|message| api_error(StatusCode::BAD_REQUEST, "file_unavailable", message))
}

pub(crate) async fn present(state: &AppState, path: String) -> Result<Value, &'static str> {
    let permit = EXPORTS
        .try_acquire()
        .map_err(|_| "File preparation is busy; try again")?;
    let pending = PendingExport(Arc::new(AtomicBool::new(false)));
    let cancelled = pending.0.clone();
    let job = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.load(Ordering::Acquire) {
            return Err("File preparation cancelled");
        }
        let home = dirs::home_dir().ok_or("Home directory unavailable")?;
        let home = home
            .canonicalize()
            .map_err(|_| "Home directory unavailable")?;
        let (bytes, name) = read_file(&path, &home)?;
        let kind = file_kind(&bytes, &name).ok_or("Unsupported file type")?;
        if cancelled.load(Ordering::Acquire) {
            return Err("File preparation cancelled");
        }
        let stored = uploads::stored_upload_name(kind);
        let dir = uploads::ensure_uploads_dir().map_err(|_| "File storage unavailable")?;
        uploads::write_upload_file(&dir.join(&stored), &bytes)
            .map_err(|_| "File storage unavailable")?;
        Ok::<_, &'static str>(
            json!({"upload_name": stored, "name": name, "mime": kind.mime, "size": bytes.len(), "expires_at_ms": crate::now_unix_ms() + uploads::UPLOAD_RETENTION.as_millis()}),
        )
    });
    // OS privacy dialogs and sleeping removable drives can block a filesystem
    // call. Bound the request while the worker keeps its concurrency permit.
    let file = tokio::time::timeout(std::time::Duration::from_secs(10), job)
        .await.map_err(|_| "File access timed out; check this computer's file permissions or external drive, then share again")?
        .map_err(|_| "File preparation failed")??;
    Ok(
        json!({"state":"done", "server_id": state.config.server_id, "machine": state.config.label, "files": [file]}),
    )
}

fn file_kind(bytes: &[u8], name: &str) -> Option<uploads::UploadKind> {
    uploads::sniff_upload_kind(bytes)
        .or_else(|| uploads::sniff_video_upload_kind(bytes))
        .or_else(|| uploads::sniff_office_upload_kind(bytes))
        .or_else(|| uploads::sniff_document_upload_kind(bytes, name))
}

/// Both the spelling supplied by the agent and the resolved target are checked.
/// This is a conservative credential-path fence, not a claim to detect every
/// secret a user may have put in an otherwise ordinary document.
fn allowed_path(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    for root in [
        "/etc",
        "/proc",
        "/sys",
        "/dev",
        "/private/etc",
        "/System",
        "/Library",
        "/root",
    ] {
        if path.starts_with(root) {
            return false;
        }
    }
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {}
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return false;
                };
                let name = name.to_ascii_lowercase();
                if name.starts_with('.')
                    || name.chars().any(char::is_control)
                    || matches!(
                        name.as_str(),
                        "library"
                            | "keychains"
                            | "credentials"
                            | "credentials.json"
                            | "secrets"
                            | "secrets.json"
                            | "secrets.yaml"
                            | "secrets.yml"
                            | "passwords"
                            | "passwords.txt"
                            | "id_rsa"
                            | "id_ed25519"
                            | "id_ecdsa"
                            | "id_dsa"
                            | "kubeconfig"
                    )
                    || [
                        ".pem",
                        ".key",
                        ".p12",
                        ".pfx",
                        ".keychain",
                        ".keychain-db",
                        ".keystore",
                        ".jks",
                        ".kdbx",
                    ]
                    .iter()
                    .any(|ext| name.ends_with(ext))
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

/// Walk the already-resolved absolute path with directory handles. A concurrent
/// replacement of an ancestor with a symlink cannot redirect the open.
#[cfg(unix)]
fn open_snapshot_source(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let mut dir = std::fs::File::open("/")?;
    let parts: Vec<_> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    for (index, part) in parts.iter().enumerate() {
        let name = std::ffi::CString::new(part.as_bytes())?;
        let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        if index + 1 < parts.len() {
            flags |= libc::O_DIRECTORY;
        }
        // SAFETY: live parent descriptor and NUL-terminated component; the
        // returned descriptor is uniquely owned by File on success.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        dir = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    Ok(dir)
}
#[cfg(not(unix))]
fn open_snapshot_source(path: &Path) -> std::io::Result<std::fs::File> {
    crate::agents::message_images::open_fenced(path, path.ancestors().last().unwrap_or(path))
}

fn read_file(raw: &str, home: &Path) -> Result<(Vec<u8>, String), &'static str> {
    let unavailable = "File is unavailable or protected; share a regular non-sensitive file";
    if raw.len() > 4096 || raw.chars().any(char::is_control) {
        return Err(unavailable);
    }
    let path = if let Some(relative) = raw.strip_prefix("~/") {
        home.join(relative)
    } else {
        raw.into()
    };
    if !allowed_path(&path) {
        return Err(unavailable);
    }
    let canonical = path.canonicalize().map_err(|_| unavailable)?;
    if !allowed_path(&canonical) {
        return Err(unavailable);
    }
    let mut file = open_snapshot_source(&canonical).map_err(|_| unavailable)?;
    let metadata = file.metadata().map_err(|_| unavailable)?;
    if !metadata.is_file() {
        return Err(unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Hard links can conceal a protected pathname. Only owner-created
        // artifacts are exported, never another account's readable files.
        if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(unavailable);
        }
    }
    let name = uploads::sanitize_upload_name(
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or(unavailable)?,
    );
    let mut head = vec![0; 8192];
    let count = file.read(&mut head).map_err(|_| unavailable)?;
    head.truncate(count);
    let video = uploads::sniff_video_upload_kind(&head).is_some();
    let limit = if video {
        uploads::MAX_VIDEO_UPLOAD_BYTES
    } else {
        uploads::MAX_UPLOAD_BYTES
    };
    if metadata.len() > limit as u64 {
        return Err("File exceeds the preview size limit (25 MiB; videos 64 MiB)");
    }
    file.rewind().map_err(|_| unavailable)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable)?;
    if bytes.len() > limit {
        return Err("File exceeds the preview size limit (25 MiB; videos 64 MiB)");
    }
    let after = file.metadata().map_err(|_| unavailable)?;
    if after.len() != metadata.len()
        || after.modified().ok() != metadata.modified().ok()
        || bytes.len() as u64 != metadata.len()
    {
        return Err("File changed while preparing the preview; share it again when saved");
    }
    let kind = file_kind(&bytes, &name).ok_or("Unsupported file type")?;
    if kind.mime.starts_with("video/") != video {
        return Err("File changed while preparing the preview; share it again when saved");
    }
    if bytes
        .windows(b"PRIVATE KEY-----".len())
        .any(|part| part == b"PRIVATE KEY-----")
    {
        return Err("Private keys cannot be shared");
    }
    Ok((bytes, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn file_handoff_requires_a_paired_caller_before_reading() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let state = crate::test_state("admin", vec![crate::test_device("phone", "paired")]);
        let app = mount(Router::new()).with_state(state);
        for token in [None, Some("wrong"), Some("paired")] {
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/mcp/files/present")
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from(r#"{"path":"/etc/passwd"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            if token == Some("paired") {
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            } else if token.is_none() {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            } else {
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
        }
    }
    #[tokio::test]
    async fn disabling_remote_tasks_also_refuses_remote_file_handoffs() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let state = crate::test_state(
            "admin",
            vec![crate::test_support::test_peer_device("peer", "peer-token")],
        );
        state.peers.set_accepts_delegations(false).unwrap();
        let response = mount(Router::new())
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/mcp/files/present")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer peer-token")
                    .body(Body::from(r#"{"path":"/tmp/report.txt"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    #[test]
    fn explicit_files_reject_escapes_hidden_files_and_directories() {
        let root =
            std::path::Path::new("/tmp").join(format!("muqun-present-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let home = root.canonicalize().unwrap();
        std::fs::write(home.join("shot.txt"), b"example").unwrap();
        assert_eq!(read_file("~/shot.txt", &home).unwrap().0, b"example");
        for path in [
            "~/../shot.txt",
            "~/.env",
            "/etc/passwd",
            "~/",
            "https://example.com/a",
        ] {
            assert!(read_file(path, &home).is_err(), "{path}");
        }
        std::fs::write(home.join(".env"), b"secret").unwrap();
        assert!(read_file("~/.env", &home).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn ordinary_symlinks_work_but_protected_targets_and_oversized_files_are_refused() {
        let root =
            std::path::Path::new("/tmp").join(format!("muqun-present-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let home = root.canonicalize().unwrap();
        std::fs::write(home.join("real.txt"), b"content").unwrap();
        std::os::unix::fs::symlink(home.join("real.txt"), home.join("alias.txt")).unwrap();
        assert_eq!(read_file("~/alias.txt", &home).unwrap().0, b"content");
        std::fs::write(home.join(".secret"), b"hidden").unwrap();
        std::os::unix::fs::symlink(home.join(".secret"), home.join("public.txt")).unwrap();
        assert!(read_file("~/public.txt", &home).is_err());
        std::fs::hard_link(home.join(".secret"), home.join("hard.txt")).unwrap();
        assert!(read_file("~/hard.txt", &home).is_err());
        let file = std::fs::File::create(home.join("large.txt")).unwrap();
        file.set_len(uploads::MAX_UPLOAD_BYTES as u64 + 1).unwrap();
        assert!(read_file("~/large.txt", &home).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn external_artifacts_are_allowed_without_following_protected_or_special_files() {
        let root =
            std::path::Path::new("/tmp").join(format!("muqun-external-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let external = root.join("external");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let home = home.canonicalize().unwrap();
        let external = external.canonicalize().unwrap();
        std::fs::write(external.join("report.txt"), b"report").unwrap();
        std::os::unix::fs::symlink(&external, home.join("Work")).unwrap();
        assert_eq!(read_file("~/Work/report.txt", &home).unwrap().0, b"report");
        assert_eq!(
            read_file(external.join("report.txt").to_str().unwrap(), &home)
                .unwrap()
                .0,
            b"report"
        );
        std::fs::write(
            external.join("report.txt"),
            b"-----BEGIN OPENSSH PRIVATE KEY-----\nsecret",
        )
        .unwrap();
        assert!(read_file("~/Work/report.txt", &home).is_err());
        std::fs::write(external.join("credentials.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(external.join("credentials.json"), home.join("export.json"))
            .unwrap();
        assert!(read_file("~/export.json", &home).is_err());
        let fifo = std::ffi::CString::new(home.join("pipe.txt").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(read_file("~/pipe.txt", &home).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
