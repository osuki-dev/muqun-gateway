//! Paired, project-scoped task inputs. Upload receipts never expose blob paths.
use axum::extract::{multipart::MultipartRejection, Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::work::model::{FailureCode, InputReceipt, InputUpload, WorkError, WorkResult};
use crate::work_http::work_error;
use crate::{ApiResult, AppState};

const MAX_INPUT_BYTES: usize = 10 * 1024 * 1024;

fn invalid() -> (StatusCode, Json<serde_json::Value>) {
    work_error(WorkError(FailureCode::InvalidInput))
}
struct ParsedInput {
    key: String,
    project: String,
    name: String,
    bytes: Vec<u8>,
}
async fn parse(mut multipart: Multipart) -> ApiResult<ParsedInput> {
    let (mut key, mut project, mut file) = (None, None, None);
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(crate::upload_body_error)?
    {
        let field_name = field.name().ok_or_else(invalid)?.to_owned();
        let limit = match field_name.as_str() {
            "request_key" if key.is_none() => 128,
            "repo_path" if project.is_none() => 4096,
            "file" if file.is_none() => MAX_INPUT_BYTES,
            _ => return Err(invalid()),
        };
        let filename = field.file_name().map(str::to_owned);
        if (field_name == "file") != filename.is_some() {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(crate::upload_body_error)? {
            if chunk.len() > limit.saturating_sub(bytes.len()) {
                return Err(if field_name == "file" {
                    crate::api_error(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "resource_limit",
                        "Task input files must be at most 10 MiB.",
                    )
                } else {
                    invalid()
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if field_name == "file" {
            file = Some((filename.unwrap(), bytes));
        } else {
            let text = String::from_utf8(bytes).map_err(|_| invalid())?;
            if text.trim().is_empty() || text.contains('\0') {
                return Err(invalid());
            }
            if field_name == "request_key" {
                key = Some(text);
            } else {
                project = Some(text);
            }
        }
    }
    let (name, bytes) = file.ok_or_else(invalid)?;
    Ok(ParsedInput {
        key: key.ok_or_else(invalid)?,
        project: project.ok_or_else(invalid)?,
        name,
        bytes,
    })
}

// The devices guard serializes revocation with receipt reads and commits. Blob
// publication has already ended; no store lock spans upload or filesystem reads.
pub(super) async fn authorized_store<T: Send + 'static>(
    state: AppState,
    headers: HeaderMap,
    actor: String,
    action: impl FnOnce(&mut crate::work::store::WorkStore) -> WorkResult<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(move || {
        let token = crate::bearer_token(&headers)?;
        let devices = crate::lock_devices(&state)?;
        if crate::identify_device(&devices, token).as_deref() != Some(actor.as_str()) {
            return Err(crate::api_error(
                StatusCode::FORBIDDEN,
                "invalid_token",
                "Invalid token.",
            ));
        }
        if let Some(key) = devices
            .iter()
            .find(|d| d.id == actor)
            .and_then(|d| d.transport_key.as_deref())
        {
            let proof = headers
                .get(crate::TRANSPORT_PROOF_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if !crate::authority::authenticates_admin(&crate::hash_token(key), proof) {
                return Err(crate::api_error(
                    StatusCode::FORBIDDEN,
                    "device_proof_required",
                    "Encrypted device proof is required.",
                ));
            }
        }
        let store = state
            .work
            .as_ref()
            .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
        let mut store = store
            .lock()
            .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?;
        action(&mut store).map_err(work_error)
    })
    .await
    .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
}

pub(super) async fn upload(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<InputReceipt>> {
    let actor = crate::require_device(&state, &headers)?;
    let config = crate::find_session(&state.config, &session)?;
    let parsed = parse(multipart.map_err(|_| invalid())?).await?;
    let kind = crate::validate_upload_content(&parsed.name, &parsed.bytes)?;
    let roots = crate::task_repo_roots(&state, config).await;
    let project = crate::tasks::resolve_repo_path(&parsed.project, &roots)
        .and_then(|p| p.to_str().map(str::to_owned))
        .ok_or_else(|| work_error(WorkError(FailureCode::ScopeMismatch)))?;
    let input = InputUpload {
        repo_path: project,
        name: crate::sanitize_upload_name(&parsed.name),
        mime: kind.mime.into(),
        size_bytes: parsed.bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&parsed.bytes)),
    };
    let (who, sid, key, metadata) = (
        actor.clone(),
        session.clone(),
        parsed.key.clone(),
        input.clone(),
    );
    if let Some(receipt) =
        authorized_store(state.clone(), headers.clone(), actor.clone(), move |s| {
            s.replay_input_upload(&who, &sid, &key, &metadata)
        })
        .await?
    {
        return Ok(Json(receipt));
    }
    let root = state
        .work_artifacts
        .clone()
        .ok_or_else(|| work_error(WorkError(FailureCode::StorageUnavailable)))?;
    let expected = crate::work::model::ArtifactRef {
        path: input.sha256.clone(),
        sha256: input.sha256.clone(),
        size_bytes: input.size_bytes,
    };
    tokio::task::spawn_blocking(move || {
        crate::work_artifacts::publish_bytes(&root, &expected, &parsed.bytes)
    })
    .await
    .map_err(|_| work_error(WorkError(FailureCode::StorageUnavailable)))?
    .map_err(work_error)?;
    let who = actor.clone();
    authorized_store(state, headers, actor, move |s| {
        s.commit_input_upload(
            &who,
            &session,
            &parsed.key,
            input,
            crate::now_unix_ms().min(i64::MAX as u128) as i64,
        )
    })
    .await
    .map(Json)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptQuery {
    request_key: String,
}
pub(super) async fn receipt(
    State(state): State<AppState>,
    Path(session): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ReceiptQuery>,
) -> ApiResult<Json<InputReceipt>> {
    let actor = crate::require_device(&state, &headers)?;
    crate::find_session(&state.config, &session)?;
    let who = actor.clone();
    authorized_store(state, headers, actor, move |s| {
        s.get_input_receipt(&who, &session, &query.request_key)
    })
    .await
    .map(Json)
}

/// Resolve only a frozen, authorized reference; caller supplies the store-owned
/// blob root. This verifies exact bytes and MIME before exposing a local path to
/// the assistant. Host-owner mutation remains outside the filesystem contract.
pub(super) fn resolve_input(
    root: &std::path::Path,
    input: &crate::work::model::FrozenInputRef,
) -> WorkResult<std::path::PathBuf> {
    if input.size_bytes > MAX_INPUT_BYTES as u64 {
        return Err(WorkError(FailureCode::ResourceLimit));
    }
    let artifact = crate::work::model::ArtifactRef {
        path: input.sha256.clone(),
        sha256: input.sha256.clone(),
        size_bytes: input.size_bytes,
    };
    let bytes = crate::work_artifacts::retrieve(root, &artifact)?;
    let kind = crate::validate_upload_content(&input.name, &bytes)
        .map_err(|_| WorkError(FailureCode::ArtifactChanged))?;
    if kind.mime != input.mime {
        return Err(WorkError(FailureCode::ArtifactChanged));
    }
    Ok(root.join(&input.sha256))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::DefaultBodyLimit,
        http::Request,
        routing::{get, post},
        Router,
    };
    use tower::ServiceExt;
    fn body(project: &str, key: &str, bytes: &[u8], extra: &str) -> Vec<u8> {
        let mut out = "--test-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"reference.txt\"\r\nContent-Type: application/x-executable\r\n\r\n".as_bytes().to_vec();
        out.extend_from_slice(bytes);
        out.extend_from_slice(format!("\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"repo_path\"\r\n\r\n{project}\r\n--test-boundary\r\nContent-Disposition: form-data; name=\"request_key\"\r\n\r\n{key}\r\n{extra}--test-boundary--\r\n").as_bytes());
        out
    }
    fn app(state: AppState) -> Router {
        Router::new()
            .route(
                "/api/sessions/{session}/work/inputs",
                post(upload).layer(DefaultBodyLimit::max(crate::MAX_UPLOAD_BYTES)),
            )
            .route("/api/sessions/{session}/work/input-receipts", get(receipt))
            .with_state(state)
    }
    async fn send(
        app: Router,
        path: &str,
        token: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("authorization", format!("Bearer {token}"))
                    .header(
                        "content-type",
                        "multipart/form-data; boundary=test-boundary",
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), crate::MAX_UPLOAD_BYTES)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn scoped_upload_rejects_invalid_fields_sizes_and_unpaired_before_storage() {
        let state =
            crate::tests::test_state("admin", vec![crate::tests::test_device("phone", "device")]);
        let path = format!("/api/sessions/{}/work/inputs", state.config.sessions[0].id);
        let router = app(state);
        let duplicate = "--test-boundary\r\nContent-Disposition: form-data; name=\"request_key\"\r\n\r\nagain\r\n";
        let unknown =
            "--test-boundary\r\nContent-Disposition: form-data; name=\"actor\"\r\n\r\nphone\r\n";
        for extra in [duplicate, unknown] {
            assert_eq!(
                send(
                    router.clone(),
                    &path,
                    "device",
                    body("/tmp", "key", b"hello", extra)
                )
                .await
                .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            send(
                router.clone(),
                &path,
                "device",
                body("/tmp", "key", &vec![b'x'; MAX_INPUT_BYTES + 1], "")
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            send(
                router.clone(),
                &path,
                "device",
                body("/tmp", "key", b"#!/bin/sh\necho hi", "")
            )
            .await
            .0,
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
        assert_eq!(
            send(router, &path, "admin", b"not multipart".to_vec())
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn scoped_upload_receipts_replay_and_remain_actor_bound_after_revocation() {
        let root = std::env::temp_dir().join(format!("work-input-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("project")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let project = root.join("project").to_string_lossy().to_string();
        let mut state = crate::tests::test_state(
            "admin",
            vec![
                crate::tests::test_device("phone", "device"),
                crate::tests::test_device("other", "other-token"),
            ],
        );
        state.work_artifacts = Some(root.join("blobs"));
        let session = state.config.sessions[0].id.clone();
        state.assets.lock().unwrap().remember_roots(
            &session,
            None,
            vec![crate::AssetRoot {
                path: root.join("project"),
                session_id: session.clone(),
                workspace_id: None,
                tab_id: None,
                pane_id: None,
            }],
        );
        let path = format!("/api/sessions/{session}/work/inputs");
        let router = app(state.clone());
        let (status, first) = send(
            router.clone(),
            &path,
            "device",
            body(&project, "same", b"actual reference", ""),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["mime"], "text/plain; charset=utf-8");
        assert!(first.get("path").is_none());
        let (_, replayed) = send(
            router.clone(),
            &path,
            "device",
            body(&project, "same", b"actual reference", ""),
        )
        .await;
        assert_eq!(replayed, first);
        assert_eq!(
            send(
                router.clone(),
                &path,
                "device",
                body(&project, "same", b"changed reference", "")
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            send(
                router.clone(),
                &path,
                "device",
                body(root.to_str().unwrap(), "foreign", b"hello", "")
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (limit_status, limit_receipt) = send(
            router.clone(),
            &path,
            "device",
            body(&project, "at-limit", &vec![b'x'; MAX_INPUT_BYTES], ""),
        )
        .await;
        assert_eq!(limit_status, StatusCode::OK);
        assert_eq!(limit_receipt["size_bytes"], MAX_INPUT_BYTES);
        let receipt_path = format!("/api/sessions/{session}/work/input-receipts?request_key=same");
        for (token, expected) in [
            ("other-token", StatusCode::NOT_FOUND),
            ("device", StatusCode::OK),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(&receipt_path)
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        state.devices.lock().unwrap().retain(|d| d.id != "phone");
        // Simulate the final commit of an upload whose bytes were already
        // published when this device was revoked. It must not create metadata.
        let mut captured_headers = HeaderMap::new();
        captured_headers.insert("authorization", "Bearer device".parse().unwrap());
        let denied = authorized_store(
            state.clone(),
            captured_headers,
            "phone".into(),
            |_store| -> WorkResult<()> { panic!("revoked upload reached metadata commit") },
        )
        .await;
        assert_eq!(denied.unwrap_err().0, StatusCode::FORBIDDEN);

        let response = router
            .oneshot(
                Request::builder()
                    .uri(receipt_path)
                    .header("authorization", "Bearer device")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn immutable_input_resolution_rejects_changed_missing_bytes_and_mime() {
        let root =
            std::env::temp_dir().join(format!("work-input-resolve-{}", uuid::Uuid::new_v4()));
        let data = b"reference content";
        let digest = format!("{:x}", Sha256::digest(data));
        let mut input = crate::work::model::FrozenInputRef {
            input_id: uuid::Uuid::new_v4().to_string(),
            caption: "untrusted caption".into(),
            use_: Default::default(),
            name: "reference.txt".into(),
            mime: "text/plain; charset=utf-8".into(),
            size_bytes: data.len() as u64,
            sha256: digest.clone(),
        };
        let expected = crate::work::model::ArtifactRef {
            path: digest.clone(),
            sha256: digest.clone(),
            size_bytes: data.len() as u64,
        };
        crate::work_artifacts::publish_bytes(&root, &expected, data).unwrap();
        assert_eq!(resolve_input(&root, &input).unwrap(), root.join(&digest));
        input.mime = "image/png".into();
        assert_eq!(
            resolve_input(&root, &input).unwrap_err().0,
            FailureCode::ArtifactChanged
        );
        input.mime = "text/plain; charset=utf-8".into();
        std::fs::write(root.join(&digest), b"changed").unwrap();
        assert_eq!(
            resolve_input(&root, &input).unwrap_err().0,
            FailureCode::ArtifactChanged
        );
        std::fs::remove_file(root.join(&digest)).unwrap();
        assert_eq!(
            resolve_input(&root, &input).unwrap_err().0,
            FailureCode::ArtifactMissing
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_exact_scoped_input_post_gets_upload_sized_encrypted_body() {
        let path = "/api/sessions/opaque/work/inputs";
        assert_eq!(
            crate::plaintext_method_body_limit(&axum::http::Method::POST, path),
            crate::MAX_UPLOAD_BYTES
        );
        for (method, path) in [
            ("GET", path),
            ("POST", "/api/sessions//work/inputs"),
            ("POST", "/api/sessions/a/work/inputs/"),
            ("POST", "/api/sessions/a/work/inputs-extra"),
            ("POST", "/api/sessions/a/work/inputs/x"),
        ] {
            assert_eq!(
                crate::plaintext_method_body_limit(&method.parse().unwrap(), path),
                crate::MAX_REQUEST_BODY_BYTES
            );
        }
    }
}
