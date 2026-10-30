//! Uploaded images are native Pi RPC images, not instructions to read a path.

use serde_json::{json, Value};

use crate::agents::adapters::acp::operations::{prompt_parts, PreparedPrompt, PromptCapabilities};
use crate::agents::ports::AgentError;

const MAX_PROMPT_BYTES: usize = 7 * 1024 * 1024;

pub(super) struct PiPrompt {
    message: String,
    images: Vec<Value>,
}

pub(super) async fn prepare(text: &str, attachments: &[String]) -> Result<PiPrompt, AgentError> {
    // This shared upload reader supplies bounded nonblocking regular-file
    // reads, the Gateway uploads fence and content-based image MIME types.
    let prepared = prompt_parts(
        text,
        attachments,
        PromptCapabilities {
            images: true,
            embedded_context: true,
        },
    )
    .await
    .map_err(|error| match error {
        AgentError::InvalidRequest(reason) => {
            AgentError::InvalidRequest(reason.replace("ACP", "Pi"))
        }
        other => other,
    })?;
    Ok(PiPrompt::from_parts(prepared))
}

impl PiPrompt {
    fn from_parts(prepared: PreparedPrompt) -> Self {
        let mut message = prepared.transcript;
        let mut images = Vec::new();
        for part in prepared.content {
            match part.get("type").and_then(Value::as_str) {
                Some("image") => images.push(part),
                Some("resource") => {
                    if let Some(text) = part.pointer("/resource/text").and_then(Value::as_str) {
                        let uri = part
                            .pointer("/resource/uri")
                            .and_then(Value::as_str)
                            .unwrap_or("uploaded text");
                        message.push_str(&format!("\n\nFile content ({uri}):\n{text}"));
                    }
                }
                _ => {}
            }
        }
        PiPrompt { message, images }
    }
}

impl PiPrompt {
    pub(super) fn command(
        self,
        kind: &str,
        streaming_behavior: Option<&str>,
    ) -> Result<Value, AgentError> {
        let mut command = json!({"type":kind,"message":self.message});
        if !self.images.is_empty() {
            command["images"] = Value::Array(self.images);
        }
        if let Some(behavior) = streaming_behavior {
            command["streamingBehavior"] = json!(behavior);
        }
        // Include text, paths, MIME names, JSON escaping and image expansion.
        // The remaining 1 MiB of the RPC limit covers IDs and native echoes.
        if serde_json::to_vec(&command)
            .map_err(|_| AgentError::InvalidRequest("invalid Pi prompt".into()))?
            .len()
            > MAX_PROMPT_BYTES
        {
            return Err(AgentError::InvalidRequest(
                "Pi encoded prompt exceeds the 7 MiB byte budget".into(),
            ));
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::acp::operations::upload_parts;
    use base64::Engine as _;

    fn png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=").unwrap()
    }

    fn webp() -> Vec<u8> {
        use image::ImageEncoder as _;
        let mut bytes = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
            .write_image(&[255, 102, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn scratch() -> Scratch {
        let root = std::env::temp_dir().join(format!("muqun-pi-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Scratch(root)
    }

    #[test]
    fn uploaded_pixels_are_native_images_for_idle_steer_and_followup_commands() {
        let root = scratch();
        let path = root.0.join("renamed-not-an-image.txt");
        std::fs::write(&path, png()).unwrap();
        for (kind, streaming) in [
            ("prompt", None),
            ("steer", None),
            ("prompt", Some("followUp")),
        ] {
            let prepared = upload_parts(
                "Identify the pixels without tools",
                &[path.to_string_lossy().into_owned()],
                &root.0,
                PromptCapabilities {
                    images: true,
                    embedded_context: true,
                },
            )
            .unwrap();
            let command = PiPrompt::from_parts(prepared)
                .command(kind, streaming)
                .unwrap();
            assert_eq!(command["type"], kind);
            assert_eq!(command["images"][0]["type"], "image");
            assert_eq!(command["images"][0]["mimeType"], "image/png");
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(command["images"][0]["data"].as_str().unwrap())
                    .unwrap(),
                png()
            );
            assert!(command["message"]
                .as_str()
                .unwrap()
                .contains(path.to_str().unwrap()));
            assert!(
                serde_json::to_vec(&command).unwrap().len() < super::super::rpc::MAX_LINE_BYTES
            );
        }
    }

    #[test]
    fn entire_encoded_frame_is_bounded_including_text_names_and_json_escaping() {
        assert!(PiPrompt {
            message: "\"".repeat(MAX_PROMPT_BYTES / 2),
            images: vec![]
        }
        .command("prompt", Some("followUp"))
        .is_err());
        assert!(PiPrompt {
            message: "x".repeat(MAX_PROMPT_BYTES),
            images: vec![json!({"type":"image","mimeType":"image/webp","data":"a".repeat(100)})]
        }
        .command("prompt", None)
        .is_err());
        assert!(PiPrompt {
            message: "x".repeat(MAX_PROMPT_BYTES - 256),
            images: vec![]
        }
        .command("prompt", Some("followUp"))
        .is_ok());
    }

    #[test]
    fn uploaded_webp_uses_its_content_mime_and_original_pixels() {
        let root = scratch();
        let path = root.0.join("camera.png");
        let pixels = webp();
        std::fs::write(&path, &pixels).unwrap();
        let prepared = upload_parts(
            "look at this",
            &[path.to_string_lossy().into_owned()],
            &root.0,
            PromptCapabilities {
                images: true,
                embedded_context: true,
            },
        )
        .unwrap();
        let command = PiPrompt::from_parts(prepared)
            .command("prompt", None)
            .unwrap();
        assert_eq!(command["images"][0]["mimeType"], "image/webp");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(command["images"][0]["data"].as_str().unwrap())
                .unwrap(),
            pixels
        );
    }

    #[test]
    fn mixed_uploads_are_fenced_and_failure_does_not_return_a_partial_prompt() {
        let root = scratch();
        let uploads = root.0.join("uploads");
        std::fs::create_dir(&uploads).unwrap();
        let image = uploads.join("ok.png");
        std::fs::write(&image, png()).unwrap();
        let outside = root.0.join("outside.png");
        std::fs::write(&outside, png()).unwrap();
        let caps = PromptCapabilities {
            images: true,
            embedded_context: true,
        };
        assert!(upload_parts(
            "prompt",
            &[
                image.to_string_lossy().into_owned(),
                outside.to_string_lossy().into_owned()
            ],
            &uploads,
            caps
        )
        .is_err());
        #[cfg(unix)]
        {
            let link = uploads.join("escape.png");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(upload_parts(
                "prompt",
                &[link.to_string_lossy().into_owned()],
                &uploads,
                caps
            )
            .is_err());
        }
        let too_many = vec![image.to_string_lossy().into_owned(); 17];
        assert!(upload_parts("prompt", &too_many, &uploads, caps).is_err());
        let oversize = uploads.join("oversize.png");
        let mut bytes = png();
        bytes.resize(4 * 1024 * 1024, 0);
        std::fs::write(&oversize, bytes).unwrap();
        assert!(upload_parts(
            "prompt",
            &[
                image.to_string_lossy().into_owned(),
                oversize.to_string_lossy().into_owned()
            ],
            &uploads,
            caps
        )
        .is_err());
    }
}
