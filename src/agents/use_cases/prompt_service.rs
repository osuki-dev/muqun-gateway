use std::path::Path;
use std::sync::Arc;

use crate::agents::domain::AgentSessionId;
use crate::agents::ports::agent::{append_attachment_paths, AgentError, AgentPort, AttachmentMode};
use crate::agents::ports::mirror::SessionMirrorPort;

pub struct PromptService {
    agent: Arc<dyn AgentPort>,
    _mirror: Arc<dyn SessionMirrorPort>,
}

impl PromptService {
    pub fn new(agent: Arc<dyn AgentPort>, _mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { agent, _mirror }
    }

    pub async fn send_prompt(
        &self,
        asid: &AgentSessionId,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<(), AgentError> {
        if attachments.is_empty() || self.agent.attachment_mode() == AttachmentMode::Native {
            return self
                .agent
                .send_prompt(&asid.0, text, attachments, delivery)
                .await;
        }
        let uploads = crate::uploads_dir()
            .map_err(|_| AgentError::InvalidRequest("no upload directory".to_string()))?;
        let text = prompt_with_attachment_paths(text, attachments, &uploads)?;
        self.agent.send_prompt(&asid.0, &text, &[], delivery).await
    }

    pub async fn interrupt(&self, asid: &AgentSessionId) -> Result<(), AgentError> {
        self.agent.interrupt(&asid.0).await
    }
}

/// `text` with the attachments listed as host paths, for an agent that reads
/// files itself. Every path must be an absolute path to a regular file inside
/// `uploads` once symlinks and `..` are resolved, so a client can only point
/// the agent at files it uploaded through `POST /api/uploads`.
pub(crate) fn prompt_with_attachment_paths(
    text: &str,
    attachments: &[String],
    uploads: &Path,
) -> Result<String, AgentError> {
    if attachments.is_empty() {
        return Ok(text.to_string());
    }
    let refuse = |path: &str| {
        AgentError::InvalidRequest(format!(
            "attachment is not a file uploaded to this gateway: {path}"
        ))
    };
    let root = uploads
        .canonicalize()
        .map_err(|_| refuse(&attachments[0]))?;
    let paths = attachments
        .iter()
        .map(|raw| {
            let path = Path::new(raw);
            if !path.is_absolute() {
                return Err(refuse(raw));
            }
            let resolved = path.canonicalize().map_err(|_| refuse(raw))?;
            if !resolved.starts_with(&root) || !resolved.is_file() {
                return Err(refuse(raw));
            }
            Ok(resolved.to_string_lossy().into_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(append_attachment_paths(text, &paths))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
