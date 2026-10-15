//! Lazy, fixed original versions. One bounded in-process registry, using the
//! existing original store and blocking admission; no per-range whole-file hash.
use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use super::originals::{metadata_etag, open_fenced, original_store, publish_original};
use super::ServedImage;

const MAX_REGISTRATIONS: usize = 256;
// No eviction of acquired originals: reject additional copies, never delete a
// user's only remaining version. Existing inline-original retention is unchanged.
const MAX_STORE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_STORE_FILES: usize = 4096;
static VERSIONS: Mutex<VecDeque<Version>> = Mutex::new(VecDeque::new());

#[derive(Clone)]
pub(super) struct Version {
    id: String,
    source_fingerprint: String,
    object: ServedImage,
    pub(super) fingerprint: String,
    pub(super) digest: String,
    pub(super) size: u64,
    pub(super) mime: &'static str,
    pub(super) name: String,
}

impl Version {
    pub(super) fn etag(&self) -> String {
        format!("\"sha256-{}\"", self.digest)
    }
    pub(super) fn descriptor(&self) -> Value {
        json!({"id":self.id,"version":self.digest,"etag":self.etag(),"size":self.size,"mime":self.mime,"name":self.name,
            "url":format!("/api/agent-images/{}/original/{}",self.id,self.digest),
            "download_url":format!("/api/agent-images/{}/download/{}",self.id,self.digest)})
    }
    pub(super) fn open(&self) -> Result<(std::fs::File, std::fs::Metadata), &'static str> {
        let file = open_fenced(&self.object.path, &self.object.root)
            .map_err(|_| "image_version_unavailable")?;
        let metadata = file.metadata().map_err(|_| "image_version_unavailable")?;
        if metadata.len() != self.size || metadata_etag(&metadata) != self.fingerprint {
            return Err("image_version_changed");
        }
        Ok((file, metadata))
    }
}

pub(super) fn selected(id: &str, digest: &str) -> Result<Version, &'static str> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("image_version_unavailable");
    }
    let registry = VERSIONS
        .lock()
        .map_err(|_| "original_storage_unavailable")?;
    registry
        .iter()
        .find(|version| version.id == id && version.digest == digest)
        .cloned()
        .ok_or("image_version_unavailable")
}

struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn capacity(root: &Path, additional: u64, additional_files: usize) -> Result<(), &'static str> {
    let mut total = additional;
    let mut count = additional_files;
    for entry in std::fs::read_dir(root).map_err(|_| "original_storage_unavailable")? {
        let metadata =
            std::fs::symlink_metadata(entry.map_err(|_| "original_storage_unavailable")?.path())
                .map_err(|_| "original_storage_unavailable")?;
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
            count = count.saturating_add(1);
        }
        if total > MAX_STORE_BYTES || count > MAX_STORE_FILES {
            return Err("image_version_storage_full");
        }
    }
    if total > MAX_STORE_BYTES || count > MAX_STORE_FILES {
        Err("image_version_storage_full")
    } else {
        Ok(())
    }
}

fn source_unchanged(
    file: &std::fs::File,
    source: &ServedImage,
    fingerprint: &str,
) -> Result<(), &'static str> {
    let current = open_fenced(&source.path, &source.root).map_err(|_| "image_source_changed")?;
    if metadata_etag(&file.metadata().map_err(|_| "image_source_changed")?) != fingerprint
        || metadata_etag(&current.metadata().map_err(|_| "image_source_changed")?) != fingerprint
    {
        return Err("image_source_changed");
    }
    Ok(())
}

/// Called within the existing bounded blocking worker. Holding one registry
/// transaction makes concurrent same-source-version requests singleflight.
pub(super) fn acquire(
    id: &str,
    source: &ServedImage,
    mut file: std::fs::File,
    metadata: std::fs::Metadata,
) -> Result<Version, &'static str> {
    let mut registry = VERSIONS
        .lock()
        .map_err(|_| "original_storage_unavailable")?;
    let fingerprint = metadata_etag(&metadata);
    if let Some(index) = registry
        .iter()
        .position(|version| version.id == id && version.source_fingerprint == fingerprint)
    {
        let version = registry[index].clone();
        if version.open().is_ok() {
            return Ok(version);
        }
        // A changed stored inode is never served under its old validator.
        // A descriptor refresh may reacquire/reverify it, not silently trust it.
        registry.remove(index);
    }
    let root = original_store()?;
    let uploads = crate::platform::uploads::uploads_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok());
    // Both directories contain create-new, never-overwritten gateway originals.
    // Upload expiry/authorization is still checked by the caller on every read.
    let reusable = source.path.parent() == Some(root.as_path())
        || uploads
            .as_ref()
            .is_some_and(|root| source.path.parent() == Some(root.as_path()));
    if !reusable {
        capacity(&root, metadata.len(), 1)?;
    }
    let pending = if reusable {
        None
    } else {
        Some(Pending(
            root.join(format!("{}.pending", uuid::Uuid::new_v4())),
        ))
    };
    let mut output = match &pending {
        Some(pending) => {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            Some(
                options
                    .open(&pending.0)
                    .map_err(|_| "original_storage_unavailable")?,
            )
        }
        None => None,
    };
    let mut head = Vec::new();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; crate::ASSET_CONTENT_CHUNK_BYTES];
    let mut total = 0u64;
    // One lookahead catches growth without allowing an unbounded growing source
    // to consume disk. The acquired target bytes, not an instantaneous source
    // snapshot, are the content version being promised.
    let mut limited = (&mut file).take(metadata.len().saturating_add(1));
    loop {
        let read = limited
            .read(&mut buffer)
            .map_err(|_| "image_source_changed")?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > metadata.len() {
            return Err("image_source_changed");
        }
        hasher.update(&buffer[..read]);
        if head.len() < crate::ASSET_SNIFF_BYTES {
            head.extend_from_slice(&buffer[..read.min(crate::ASSET_SNIFF_BYTES - head.len())]);
        }
        if let Some(output) = &mut output {
            output
                .write_all(&buffer[..read])
                .map_err(|_| "original_storage_unavailable")?;
        }
    }
    if total != metadata.len() {
        return Err("image_source_changed");
    }
    if let Some(output) = output {
        output
            .sync_all()
            .map_err(|_| "original_storage_unavailable")?;
    }
    source_unchanged(&file, source, &fingerprint)?;
    let digest = hex::encode(hasher.finalize());
    let object = if let Some(pending) = &pending {
        let extension =
            crate::platform::uploads::sniff_upload_kind(&head).map_or("bin", |kind| kind.extension);
        let path = root.join(format!("{digest}.{extension}"));
        // A matching existing original is reused, not copied a second time.
        if !path.exists() {
            capacity(&root, 0, 0)?;
        }
        let path = publish_original(&pending.0, &root, &digest, extension, total)?;
        ServedImage { path, root }
    } else {
        source.clone()
    };
    drop(pending); // unlink staging before recording the published inode's ctime
    let object_file =
        open_fenced(&object.path, &object.root).map_err(|_| "original_storage_unavailable")?;
    let object_metadata = object_file
        .metadata()
        .map_err(|_| "original_storage_unavailable")?;
    if object_metadata.len() != total
        || (reusable && metadata_etag(&object_metadata) != fingerprint)
    {
        return Err("image_version_changed");
    }
    let name = crate::platform::assets::header_safe_name(
        source
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("original"),
    );
    let mime = crate::sniff_asset_type(&head, &name).mime;
    let version = Version {
        id: id.into(),
        source_fingerprint: fingerprint,
        object,
        fingerprint: metadata_etag(&object_metadata),
        digest,
        size: total,
        mime,
        name,
    };
    registry.retain(|existing| !(existing.id == version.id && existing.digest == version.digest));
    registry.push_back(version.clone());
    while registry.len() > MAX_REGISTRATIONS {
        registry.pop_front();
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, TimelineItem, TimelineRole};
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::Response;
    use base64::Engine as _;
    use futures::StreamExt as _;
    use std::io::Seek as _;
    use tower::ServiceExt as _;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn workspace() -> Scratch {
        Scratch(crate::test_support::asset_test_dir("image-versions"))
    }
    fn png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(37, 29)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    fn timeline(source: String) -> TimelineItem {
        TimelineItem {
            id: "version:t0".into(),
            message_id: "version".into(),
            role: TimelineRole::User,
            part: AgentPart::Text {
                text: "fixed version".into(),
            },
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: Some(vec![source]),
            image_assets: None,
        }
    }
    async fn register(
        state: &crate::AppState,
        root: Option<PathBuf>,
        source: String,
        asid: &str,
    ) -> String {
        let mut items = vec![timeline(source)];
        super::super::originals::attach(state, asid, root, &mut items).await;
        let asset = &items[0].image_assets.as_ref().unwrap()[0];
        assert!(asset.version_url.is_some());
        asset.asset_id.clone()
    }
    fn app(state: crate::AppState) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/agent-images/{id}/{variant}",
                axum::routing::get(super::super::originals::content),
            )
            .route(
                "/api/agent-images/{id}/{variant}/{version}",
                axum::routing::get(super::super::originals::version_content),
            )
            .with_state(state)
    }
    async fn request(app: &axum::Router, uri: &str, headers: HeaderMap) -> Response {
        let mut request = axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        *request.headers_mut() = headers;
        app.clone().oneshot(request).await.unwrap()
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }
    async fn descriptor(app: &axum::Router, id: &str) -> Value {
        let response = request(
            app,
            &format!("/api/agent-images/{id}/version"),
            crate::bearer_headers("token"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = json_body(response).await;
        value["data"]["asset"].clone()
    }
    fn file_digest(path: &Path) -> String {
        let mut file = std::fs::File::open(path).unwrap();
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        hex::encode(hash.finalize())
    }

    #[tokio::test]
    async fn fixed_33_mib_version_never_mixes_ranges_after_source_changes() {
        let scratch = workspace();
        let path = scratch.0.join("source.png");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&png()).unwrap();
        file.set_len(33 * 1024 * 1024).unwrap();
        drop(file);
        let expected = file_digest(&path);
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let id = register(
            &state,
            Some(scratch.0.clone()),
            path.to_string_lossy().into_owned(),
            "fixed-33m",
        )
        .await;
        let app = app(state.clone());
        let info = descriptor(&app, &id).await;
        let version = info["version"].as_str().unwrap();
        let etag = info["etag"].as_str().unwrap();
        let url = info["download_url"].as_str().unwrap();
        assert_eq!(version, expected);
        assert_eq!(etag, format!("\"sha256-{expected}\""));
        assert_eq!(info["size"], 33 * 1024 * 1024);
        assert!(!serde_json::to_string(&info).unwrap().contains("file://"));
        let mut hash = Sha256::new();
        let mut total = 0;
        for start in (0..33 * 1024 * 1024u64).step_by(8 * 1024 * 1024) {
            let end = (start + 8 * 1024 * 1024 - 1).min(33 * 1024 * 1024 - 1);
            let mut headers = crate::bearer_headers("token");
            headers.insert(
                "range",
                HeaderValue::from_str(&format!("bytes={start}-{end}")).unwrap(),
            );
            headers.insert("if-match", HeaderValue::from_str(etag).unwrap());
            let response = request(&app, url, headers).await;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(response.headers()["etag"], etag);
            assert_eq!(
                response.headers()["content-range"],
                format!("bytes {start}-{end}/{}", 33 * 1024 * 1024)
            );
            assert_eq!(
                response.headers()["content-length"]
                    .to_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap(),
                end - start + 1
            );
            let mut body = response.into_body().into_data_stream();
            while let Some(chunk) = body.next().await {
                let chunk = chunk.unwrap();
                assert!(chunk.len() <= crate::ASSET_CONTENT_CHUNK_BYTES);
                total += chunk.len() as u64;
                hash.update(&chunk);
            }
            if start == 0 {
                let mut source = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
                // Change a not-yet-downloaded range: reopening the live source
                // for part one would now fail the final complete digest check.
                source.seek(std::io::SeekFrom::Start(9*1024*1024+1024)).unwrap();
                source.write_all(b"changed source after part zero").unwrap();
                source.sync_all().unwrap();
                let fresh = descriptor(&app, &id).await;
                assert_ne!(fresh["version"], info["version"]);
            }
        }
        assert_eq!(total, 33 * 1024 * 1024);
        assert_eq!(hex::encode(hash.finalize()), expected);
        assert_ne!(file_digest(&path), expected);
        for condition in ["\"wrong-version\"".to_string(), format!("W/{etag}")] {
            let mut headers = crate::bearer_headers("token");
            headers.insert("range", HeaderValue::from_static("bytes=0-15"));
            headers.insert("if-match", HeaderValue::from_str(&condition).unwrap());
            assert_eq!(
                request(&app, url, headers).await.status(),
                StatusCode::PRECONDITION_FAILED
            );
        }
        let mut headers = crate::bearer_headers("token");
        headers.insert("range", HeaderValue::from_static("bytes=0-15"));
        headers.insert(
            "if-range",
            HeaderValue::from_str(&format!("W/{etag}")).unwrap(),
        );
        assert_eq!(
            request(&app, url, headers).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        let mut headers = crate::bearer_headers("token");
        headers.insert("range", HeaderValue::from_static("bytes=0-15"));
        headers.insert(
            "if-range",
            HeaderValue::from_static("\"different-strong-version\""),
        );
        let full = request(&app, url, headers).await;
        assert_eq!(full.status(), StatusCode::OK);
        assert!(!full.headers().contains_key("content-range"));
        drop(full); // never append a 200 fallback
        assert_eq!(
            request(
                &app,
                &format!("/api/agent-images/{id}/thumbnail"),
                crate::bearer_headers("token")
            )
            .await
            .status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            request(
                &app,
                &format!("/api/agent-images/{id}/original"),
                crate::bearer_headers("token")
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(&app, url, HeaderMap::new()).await.status(),
            StatusCode::UNAUTHORIZED
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("/etc/hosts", &path).unwrap();
            assert_eq!(
                request(&app, url, crate::bearer_headers("token"))
                    .await
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn immutable_inline_and_upload_are_reused_and_same_version_requests_singleflight() {
        let state = crate::test_state("admin", vec![crate::test_device("phone", "token")]);
        let bytes = png();
        let source = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        let id = register(&state, None, source, "inline-version").await;
        let original = state.message_images.served(&id).unwrap();
        let app = app(state.clone());
        let results = futures::future::join_all((0..4).map(|_| descriptor(&app, &id))).await;
        assert!(results
            .iter()
            .all(|result| result["version"] == results[0]["version"]));
        let fixed = selected(&id, results[0]["version"].as_str().unwrap()).unwrap();
        assert_eq!(fixed.object.path, original.path);
        let uploads = crate::platform::uploads::ensure_uploads_dir().unwrap();
        let path = uploads.join(format!("{}.png", uuid::Uuid::new_v4()));
        crate::platform::uploads::write_upload_file(&path, &bytes).unwrap();
        let upload_id = register(
            &state,
            None,
            path.to_string_lossy().into_owned(),
            "upload-version",
        )
        .await;
        let unbound = format!(
            "/api/agent-images/{upload_id}/download/{}",
            results[0]["version"].as_str().unwrap()
        );
        assert_eq!(
            request(&app, &unbound, crate::bearer_headers("token"))
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let upload = descriptor(&app, &upload_id).await;
        assert_eq!(
            selected(&upload_id, upload["version"].as_str().unwrap())
                .unwrap()
                .object
                .path,
            path
        );
        let invented = format!(
            "/api/agent-images/{upload_id}/download/{}",
            results[0]["version"].as_str().unwrap()
        ); // both legitimately registered same content
        assert_eq!(
            request(&app, &invented, crate::bearer_headers("token"))
                .await
                .status(),
            StatusCode::OK
        );
        // Metadata-only change is reverified once by descriptor acquisition;
        // identical bytes retain the same content version, not a stale entry.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_modified(std::time::SystemTime::now()+std::time::Duration::from_secs(1)).unwrap();
        let refreshed=descriptor(&app,&upload_id).await;
        assert_eq!(refreshed["version"],upload["version"]);
        assert_eq!(request(&app,&invented,crate::bearer_headers("token")).await.status(),StatusCode::OK);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn quota_and_changed_source_never_publish_partial_or_delete_originals() {
        let scratch = workspace();
        let store = scratch.0.join("quota");
        std::fs::create_dir(&store).unwrap();
        let only = store.join("only-original");
        std::fs::File::create(&only)
            .unwrap()
            .set_len(MAX_STORE_BYTES)
            .unwrap();
        assert_eq!(capacity(&store, 1, 1), Err("image_version_storage_full"));
        assert_eq!(std::fs::metadata(&only).unwrap().len(), MAX_STORE_BYTES);
        let root = original_store().unwrap();
        let path = scratch.0.join("changing.bin");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(uuid::Uuid::new_v4().as_bytes()).unwrap();
        file.set_len(33 * 1024 * 1024).unwrap();
        drop(file);
        let source = ServedImage {
            path: path.clone(),
            root: scratch.0.clone(),
        };
        let file = open_fenced(&path, &source.root).unwrap();
        let metadata = file.metadata().unwrap();
        let expected = file_digest(&path);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(1024 * 1024)
            .unwrap();
        let partial = file_digest(&path);
        assert!(matches!(
            acquire("changed-copy-fixture", &source, file, metadata),
            Err("image_source_changed")
        ));
        assert!(!root.join(format!("{expected}.bin")).exists());
        assert!(!root.join(format!("{partial}.bin")).exists());
        assert_eq!(std::fs::metadata(path).unwrap().len(), 1024 * 1024);
        assert!(matches!(
            selected("changed-copy-fixture", &expected),
            Err("image_version_unavailable")
        ));
    }
}
