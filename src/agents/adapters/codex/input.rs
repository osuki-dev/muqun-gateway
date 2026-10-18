//! Stable Codex UserInput parts, exclusively from checked Gateway uploads.
use std::io::Read as _;
use std::path::Path;

use serde_json::{json, Value};

use crate::agents::ports::agent::{append_attachment_paths, AgentError, AttachmentMode};
use crate::agents::use_cases::prompt_service::checked_attachments;
use crate::platform::uploads::{sniff_upload_kind, MAX_UPLOAD_BYTES};

const MAX_ATTACHMENTS: usize = 16;

pub(super) fn text_input(text: &str) -> Vec<Value> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type":"text","text":text,"text_elements":[]})]
    }
}

pub(super) async fn prepare(text: &str, attachments: &[String]) -> Result<Vec<Value>, AgentError> {
    if attachments.is_empty() {
        return Ok(text_input(text));
    }
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid("at most 16 uploads are allowed"));
    }
    let uploads = crate::uploads_dir().map_err(|_| invalid("upload directory unavailable"))?;
    let text = text.to_owned();
    let attachments = attachments.to_vec();
    tokio::task::spawn_blocking(move || upload_input(&text, &attachments, &uploads))
        .await
        .map_err(|_| invalid("upload inspection failed"))?
}

fn invalid(reason: &str) -> AgentError {
    AgentError::InvalidRequest(format!("Codex attachment: {reason}"))
}

fn upload_input(
    text: &str,
    attachments: &[String],
    uploads: &Path,
) -> Result<Vec<Value>, AgentError> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid("at most 16 uploads are allowed"));
    }
    // Repeat the service boundary check for internal callers too. In particular
    // changing to native parts must not allow Native's arbitrary URL/data inputs.
    let checked = checked_attachments(AttachmentMode::NativeUploads, attachments, uploads)?;
    let mut images = Vec::new();
    let mut documents = Vec::new();
    let mut total = 0_u64;
    for path in checked {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(&path)
            .map_err(|_| invalid("upload is unreadable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| invalid("upload is unreadable"))?;
        total = total.saturating_add(metadata.len());
        if !metadata.is_file() || metadata.len() == 0 || total > MAX_UPLOAD_BYTES as u64 {
            return Err(invalid(
                "uploads must be regular nonempty files totaling at most 25 MiB",
            ));
        }
        let mut head = Vec::new();
        file.take(512)
            .read_to_end(&mut head)
            .map_err(|_| invalid("upload is unreadable"))?;
        match sniff_upload_kind(&head).map(|kind| kind.mime) {
            Some("image/png" | "image/jpeg" | "image/webp" | "image/gif") => {
                // The verified daemon/private app-server is on the same host.
                // Official localImage loads image bytes; a text path does not.
                images.push(json!({"type":"localImage","path":path}));
            }
            Some(_) => {
                return Err(invalid(
                    "image format is unsupported; upload PNG, JPEG, WebP or GIF",
                ))
            }
            None => {
                let extension = Path::new(&path)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                if ["png", "jpg", "jpeg", "webp", "gif", "heic", "heif"]
                    .iter()
                    .any(|e| extension.eq_ignore_ascii_case(e))
                {
                    return Err(invalid("image bytes do not match a supported format"));
                }
                documents.push(path);
            }
        }
    }
    let mut input = text_input(&append_attachment_paths(text, &documents));
    input.extend(images);
    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::{
        tests::{golden, Fixture},
        CodexDriver,
    };
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::domain::AgentSessionId;
    use crate::agents::ports::agent::AgentPort;
    use crate::agents::use_cases::PromptService;
    use std::sync::{Arc, Mutex};

    struct Files(Vec<std::path::PathBuf>);
    impl Files {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn add(&mut self, dir: &Path, extension: &str, bytes: &[u8]) -> String {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join(format!("{}.{}", uuid::Uuid::new_v4(), extension));
            std::fs::write(&path, bytes).unwrap();
            self.0.push(path.clone());
            path.canonicalize().unwrap().to_str().unwrap().to_owned()
        }
    }
    impl Drop for Files {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    #[test]
    fn codex_upload_parts_use_content_type_and_keep_only_documents_in_text() {
        let mut files = Files::new();
        let root = crate::ensure_uploads_dir().unwrap();
        let png = files.add(&root, "png", b"\x89PNG\r\n\x1a\nsynthetic");
        let webp = files.add(&root, "webp", b"RIFF\x24\x00\x00\x00WEBPVP8 ");
        let document = files.add(&root, "txt", b"synthetic document");
        let input = upload_input(
            "inspect",
            &[png.clone(), document.clone(), webp.clone()],
            &root,
        )
        .unwrap();
        assert_eq!(
            input,
            vec![
                json!({"type":"text","text":format!("inspect\n\nAttached files (on this host):\n- {document}"),"text_elements":[]}),
                json!({"type":"localImage","path":png}),
                json!({"type":"localImage","path":webp})
            ]
        );
        assert_eq!(
            upload_input("", std::slice::from_ref(&png), &root).unwrap(),
            vec![json!({"type":"localImage","path":png})]
        );
    }

    #[test]
    fn codex_upload_parts_refuse_external_unreadable_oversized_and_unsupported_inputs() {
        let mut files = Files::new();
        let root = crate::ensure_uploads_dir().unwrap();
        let outside = files.add(&crate::state_dir().unwrap(), "png", b"\x89PNG\r\n\x1a\n");
        let bad_image = files.add(&root, "png", b"this is not an image");
        let empty = files.add(&root, "txt", b"");
        let heic = files.add(&root, "heic", b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00");
        let oversized = files.add(&root, "png", b"\x89PNG\r\n\x1a\n");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&oversized)
            .unwrap()
            .set_len(MAX_UPLOAD_BYTES as u64 + 1)
            .unwrap();
        for bad in [
            outside.clone(),
            bad_image,
            empty,
            heic,
            oversized,
            "https://example.test/x.png".into(),
            "data:image/png;base64,eA==".into(),
            format!("file://{outside}"),
            "relative.png".into(),
        ] {
            assert!(matches!(
                upload_input("inspect", &[bad], &root),
                Err(AgentError::InvalidRequest(_))
            ));
        }
        assert!(upload_input(
            "inspect",
            &vec![outside.clone(); MAX_ATTACHMENTS + 1],
            &root
        )
        .is_err());
        #[cfg(unix)]
        {
            let link = root.join(format!("{}.png", uuid::Uuid::new_v4()));
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            files.0.push(link.clone());
            assert!(upload_input("inspect", &[link.to_str().unwrap().into()], &root).is_err());
        }
    }

    #[tokio::test]
    async fn codex_bad_upload_fails_before_any_session_read_resume_or_turn() {
        let mut files = Files::new();
        let root = crate::ensure_uploads_dir().unwrap();
        let bad = files.add(&root, "png", b"not an image");
        let mut fixture = Fixture::new(|_| panic!("invalid image must not cause an RPC")).await;
        fixture.ready().await;
        let driver = CodexDriver::new(fixture.client.clone());
        for delivery in [None, Some("steer")] {
            assert!(matches!(
                driver
                    .send_prompt(
                        "historical",
                        "inspect",
                        std::slice::from_ref(&bad),
                        delivery
                    )
                    .await,
                Err(AgentError::InvalidRequest(_))
            ));
        }
    }

    #[tokio::test]
    async fn codex_prompt_service_sends_native_images_in_start_and_steer() {
        let mut files = Files::new();
        let root = crate::ensure_uploads_dir().unwrap();
        let path = files.add(&root, "webp", b"RIFF\x24\x00\x00\x00WEBPVP8 ");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let mut fixture = Fixture::new(move |r| match r["method"].as_str().unwrap() {
            "thread/start" | "thread/read" => json!({"thread":golden()["thread"]}),
            "turn/start" => {
                observed.lock().unwrap().push(r.clone());
                json!({"turn":{"id":"image-turn","status":"inProgress"}})
            }
            "turn/steer" => {
                observed.lock().unwrap().push(r.clone());
                json!({"turnId":"image-turn"})
            }
            other => panic!("unexpected image-input RPC {other}"),
        })
        .await;
        fixture.ready().await;
        let driver = Arc::new(CodexDriver::new(fixture.client.clone()));
        assert_eq!(driver.attachment_mode(), AttachmentMode::NativeUploads);
        let session = driver.create_session(None, None, None).await.unwrap();
        let service = PromptService::new(driver.clone(), Arc::new(MemoryMirror::new()));
        for bad in [
            "https://example.test/x.png",
            "data:image/png;base64,eA==",
            "/synthetic/not-uploaded.png",
        ] {
            assert!(service
                .send_prompt(&session.asid, "inspect", &[bad.into()], None)
                .await
                .is_err());
        }
        assert!(seen.lock().unwrap().is_empty());
        service
            .send_prompt(&session.asid, "inspect", std::slice::from_ref(&path), None)
            .await
            .unwrap();
        service
            .send_prompt(
                &session.asid,
                "",
                std::slice::from_ref(&path),
                Some("steer"),
            )
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0]["params"]["input"],
            json!([
                {"type":"text","text":"inspect","text_elements":[]}, {"type":"localImage","path":path}
            ])
        );
        assert_eq!(
            seen[1]["params"]["input"],
            json!([{"type":"localImage","path":path}])
        );
        assert_eq!(seen[1]["params"]["expectedTurnId"], "image-turn");
        assert_eq!(
            session.asid,
            AgentSessionId(golden()["thread"]["id"].as_str().unwrap().into())
        );
    }
}
