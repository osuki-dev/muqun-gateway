use std::path::Path;
use std::sync::Arc;

use crate::agents::domain::AgentSessionId;
use crate::agents::ports::agent::{append_attachment_paths, AgentError, AgentPort, AttachmentMode};

pub struct PromptService {
    agent: Arc<dyn AgentPort>,
}

impl PromptService {
    pub fn new(agent: Arc<dyn AgentPort>) -> Self {
        Self { agent }
    }

    pub async fn send_prompt(
        &self,
        asid: &AgentSessionId,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<(), AgentError> {
        // All adapters receive the same intent. Providers with a native inbox
        // own their queue; Codex implements that contract in its adapter.
        if attachments.is_empty() {
            return self
                .agent
                .send_prompt(&asid.0, text, attachments, delivery)
                .await;
        }
        let uploads = crate::uploads_dir()
            .map_err(|_| AgentError::InvalidRequest("no upload directory".to_string()))?;
        let mode = self.agent.attachment_mode();
        let checked = checked_attachments(mode, attachments, &uploads)?;
        if mode != AttachmentMode::ByPath {
            return self
                .agent
                .send_prompt(&asid.0, text, &checked, delivery)
                .await;
        }
        let text = append_attachment_paths(text, &checked);
        self.agent.send_prompt(&asid.0, &text, &[], delivery).await
    }

    pub async fn interrupt(&self, asid: &AgentSessionId) -> Result<(), AgentError> {
        self.agent.interrupt(&asid.0).await
    }
}

/// `text` with the attachments listed as host paths, for an agent that reads
/// files itself ([`checked_attachments`]).
pub(crate) fn prompt_with_attachment_paths(
    text: &str,
    attachments: &[String],
    uploads: &Path,
) -> Result<String, AgentError> {
    let paths = checked_attachments(AttachmentMode::ByPath, attachments, uploads)?;
    Ok(append_attachment_paths(text, &paths))
}

/// The attachments as an agent of `mode` is given them. A file reference --
/// a path, or for an agent that takes files itself a `file://` URI -- must be
/// an absolute path to a regular file inside `uploads` once symlinks and `..`
/// are resolved, and is given as that canonical path, so a client can only
/// point the agent at files it uploaded through `POST /api/uploads`. An agent
/// that takes files itself also takes `http(s)://` and `data:` references as
/// they are.
pub(crate) fn checked_attachments(
    mode: AttachmentMode,
    attachments: &[String],
    uploads: &Path,
) -> Result<Vec<String>, AgentError> {
    let refuse = |path: &str| {
        AgentError::InvalidRequest(format!(
            "attachment is not a file uploaded to this gateway: {path}"
        ))
    };
    let mut root = None;
    attachments
        .iter()
        .map(|raw| {
            let native = mode == AttachmentMode::Native;
            if native
                && ["http://", "https://", "data:"]
                    .iter()
                    .any(|scheme| raw.starts_with(scheme))
            {
                return Ok(raw.clone());
            }
            let path = Path::new(if native {
                raw.strip_prefix("file://").unwrap_or(raw)
            } else {
                raw
            });
            if !path.is_absolute() {
                return Err(refuse(raw));
            }
            if root.is_none() {
                root = Some(uploads.canonicalize().map_err(|_| refuse(raw))?);
            }
            let resolved = path.canonicalize().map_err(|_| refuse(raw))?;
            if root.as_ref().is_none_or(|root| !resolved.starts_with(root)) || !resolved.is_file() {
                return Err(refuse(raw));
            }
            Ok(resolved.to_string_lossy().into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prompt_delivery_preserves_intent_for_every_provider() {
        for provider in ["codex", "opencode", "deepseek", "t3"] {
            let agent = Arc::new(crate::test_support::FakeAgent::new(provider));
            let service = PromptService::new(agent.clone());
            let id = AgentSessionId("session".into());
            for delivery in [None, Some("steer"), Some("queue")] {
                service
                    .send_prompt(&id, "keep this message", &[], delivery)
                    .await
                    .unwrap();
            }
            assert_eq!(
                *agent.deliveries.lock().unwrap(),
                vec![None, Some("steer".into()), Some("queue".into())]
            );
        }
    }

    struct Scratch(std::path::PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn mkdir(dir: std::path::PathBuf) -> std::path::PathBuf {
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// An upload directory holding `names`, and a sibling directory beside it
    /// holding a file the client never uploaded.
    fn uploads_with(names: &[&str]) -> (Scratch, std::path::PathBuf) {
        let base = Scratch(mkdir(
            std::env::temp_dir().join(format!("muqun-attach-{}", uuid::Uuid::new_v4().simple())),
        ));
        let uploads = mkdir(base.0.join("uploads"));
        for name in names {
            std::fs::write(uploads.join(name), b"x").unwrap();
        }
        std::fs::write(base.0.join("secret.txt"), b"x").unwrap();
        (base, uploads)
    }

    fn path_in(dir: &Path, name: &str) -> String {
        dir.join(name).to_string_lossy().into_owned()
    }

    #[test]
    fn uploaded_files_are_listed_after_the_text() {
        let (_base, dir) = uploads_with(&["one.png", "two.pdf"]);
        let (one, two) = (path_in(&dir, "one.png"), path_in(&dir, "two.pdf"));
        let text = prompt_with_attachment_paths("look", &[one.clone(), two.clone()], &dir).unwrap();
        assert_eq!(
            text,
            format!("look\n\nAttached files (on this host):\n- {one}\n- {two}")
        );
    }

    #[test]
    fn no_attachments_leaves_the_text_alone() {
        let (_base, dir) = uploads_with(&[]);
        assert_eq!(
            prompt_with_attachment_paths("look", &[], &dir).unwrap(),
            "look"
        );
    }

    #[test]
    fn paths_outside_the_upload_directory_are_refused() {
        let (base, dir) = uploads_with(&["one.png"]);
        for bad in [
            path_in(&base.0, "secret.txt"),
            path_in(&dir, "../secret.txt"),
            "one.png".to_string(),
            path_in(&dir, "missing.png"),
            path_in(&dir, ""),
        ] {
            let err = prompt_with_attachment_paths("look", std::slice::from_ref(&bad), &dir)
                .expect_err(&bad);
            assert!(matches!(err, AgentError::InvalidRequest(_)), "{bad}");
        }
    }

    /// An agent that takes files itself gets uploaded files as canonical
    /// paths; a `file://` or path outside the uploads is refused like any
    /// other, and a web or data reference goes as it is.
    #[test]
    fn a_native_agent_gets_only_uploaded_files() {
        let (_scratch, uploads) = uploads_with(&["shot.png"]);
        let shot = path_in(&uploads, "shot.png");
        for (raw, kept) in [
            (shot.clone(), Some(shot.clone())),
            (format!("file://{shot}"), Some(shot.clone())),
            ("file:///etc/hosts".to_owned(), None),
            ("/etc/hosts".to_owned(), None),
            (
                "https://example.com/a.png".to_owned(),
                Some("https://example.com/a.png".to_owned()),
            ),
            (
                "data:text/plain,hi".to_owned(),
                Some("data:text/plain,hi".to_owned()),
            ),
        ] {
            let got =
                checked_attachments(AttachmentMode::Native, std::slice::from_ref(&raw), &uploads);
            assert_eq!(got.ok(), kept.map(|kept| vec![kept]), "{raw}");
        }
    }
}
