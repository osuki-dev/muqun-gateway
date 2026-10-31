//! Native ACP prompt parts and discovered commands. Upload authorization remains
//! the existing Gateway boundary; these helpers never fetch remote references.
use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::agents::domain::CommandInfo;
use crate::agents::ports::agent::{append_attachment_paths, AgentError, AttachmentMode};
use crate::agents::use_cases::prompt_service::checked_attachments;

const MAX_ATTACHMENTS: usize = 16;
const MAX_IMAGE_ENCODED_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = MAX_IMAGE_ENCODED_BYTES / 4 * 3;
// Leave bounded room for the JSON-RPC envelope inside the native 8 MiB frame.
const MAX_PROMPT_BYTES: usize = 7 * 1024 * 1024;
const MAX_COMMANDS: usize = 512;
const MAX_COMMAND_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Default)]
pub(crate) struct PromptCapabilities {
    pub images: bool,
    pub embedded_context: bool,
}

pub(crate) struct PreparedPrompt {
    pub content: Vec<Value>,
    pub transcript: String,
}

fn invalid(message: &str) -> AgentError {
    AgentError::InvalidRequest(message.into())
}

pub(crate) async fn prompt_parts(
    text: &str,
    attachments: &[String],
    capabilities: PromptCapabilities,
) -> Result<PreparedPrompt, AgentError> {
    if text.len() > MAX_PROMPT_BYTES {
        return Err(invalid("ACP text exceeds the prompt byte budget"));
    }
    if attachments.is_empty() {
        let content = vec![json!({"type":"text","text":text})];
        prompt_budget(&content)?;
        return Ok(PreparedPrompt {
            content,
            transcript: text.into(),
        });
    }
    let uploads = crate::uploads_dir().map_err(|_| invalid("no upload directory"))?;
    let text = text.to_owned();
    let attachments = attachments.to_vec();
    tokio::task::spawn_blocking(move || upload_parts(&text, &attachments, &uploads, capabilities))
        .await
        .map_err(|_| AgentError::NotAvailable("ACP attachment preparation failed".into()))?
}

pub(crate) fn upload_parts(
    text: &str,
    attachments: &[String],
    uploads: &Path,
    capabilities: PromptCapabilities,
) -> Result<PreparedPrompt, AgentError> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid("at most 16 uploads are allowed"));
    }
    let paths = checked_attachments(AttachmentMode::NativeUploads, attachments, uploads)?;
    let transcript = append_attachment_paths(text, &paths);
    let mut content = vec![json!({"type":"text","text":transcript})];
    let mut total = transcript.len();
    for path in &paths {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options
            .open(path)
            .map_err(|_| invalid("upload is unreadable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| invalid("upload is unreadable"))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_PROMPT_BYTES as u64 {
            return Err(invalid(
                "ACP uploads must be bounded nonempty regular files",
            ));
        }
        let length = metadata.len() as usize;
        // Bound cumulative reads before allocating, including unsupported inputs.
        total = total.saturating_add(length);
        if total > MAX_PROMPT_BYTES {
            return Err(invalid("ACP attachments exceed the prompt byte budget"));
        }
        let mut bytes = Vec::with_capacity(length);
        (&mut file)
            .take((length + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("upload is unreadable"))?;
        if bytes.len() != length {
            return Err(invalid("upload changed while preparing the prompt"));
        }
        if let Some(kind) = crate::platform::uploads::sniff_upload_kind(&bytes) {
            if !capabilities.images {
                return Err(AgentError::Unsupported(
                    "this ACP agent did not advertise image prompts".into(),
                ));
            }
            if !matches!(
                kind.mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) {
                return Err(invalid(
                    "ACP image format is unsupported; upload PNG, JPEG, GIF or WebP",
                ));
            }
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(invalid(
                    "ACP image exceeds the native 5 MiB encoded image limit",
                ));
            }
            content.push(json!({"type":"image","mimeType":kind.mime,"data":base64::engine::general_purpose::STANDARD.encode(&bytes)}));
        } else {
            if !capabilities.embedded_context {
                return Err(AgentError::Unsupported(
                    "this ACP agent did not advertise embedded file content".into(),
                ));
            }
            if bytes.starts_with(b"%PDF-") || !crate::looks_textual(&bytes) {
                return Err(invalid(
                    "this ACP agent accepts images and UTF-8 text uploads, not binary documents",
                ));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| invalid("ACP text uploads must be valid UTF-8"))?;
            let uri = reqwest::Url::from_file_path(path)
                .map_err(|_| invalid("upload has no file URI"))?;
            content.push(json!({"type":"resource","resource":{"uri":uri.as_str(),"mimeType":"text/plain","text":text}}));
        }
    }
    prompt_budget(&content)?;
    Ok(PreparedPrompt {
        content,
        transcript,
    })
}

fn prompt_budget(content: &[Value]) -> Result<(), AgentError> {
    if serde_json::to_vec(content)
        .map_err(|_| invalid("invalid ACP prompt"))?
        .len()
        > MAX_PROMPT_BYTES
    {
        return Err(invalid(
            "ACP encoded content exceeds the prompt byte budget",
        ));
    }
    Ok(())
}

fn command_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('/')
        && !name.chars().any(|ch| ch.is_control() || ch.is_whitespace())
}

pub(crate) fn commands(update: &Value) -> Result<Vec<CommandInfo>, AgentError> {
    let rows = update["availableCommands"]
        .as_array()
        .ok_or_else(|| AgentError::Protocol("ACP commands update has no command list".into()))?;
    if rows.len() > MAX_COMMANDS
        || serde_json::to_vec(rows).map_or(true, |bytes| bytes.len() > MAX_COMMAND_BYTES)
    {
        return Err(AgentError::Protocol(
            "ACP command catalog exceeds its budget".into(),
        ));
    }
    let mut names = HashSet::new();
    let mut commands = Vec::new();
    for row in rows {
        let name = row["name"]
            .as_str()
            .filter(|name| command_name(name))
            .ok_or_else(|| AgentError::Protocol("ACP command has an invalid name".into()))?;
        // Cursor lists a global command and a user skill under one name;
        // both run as the same `/name`, so the first description is kept.
        if !names.insert(name) {
            continue;
        }
        commands.push(CommandInfo {
            name: name.into(),
            description: row["description"].as_str().map(str::to_owned),
            mode: None,
            template: row
                .pointer("/input/hint")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    Ok(commands)
}

pub(crate) fn command_prompt(
    offered: &[CommandInfo],
    name: &str,
    arguments: &str,
) -> Result<String, AgentError> {
    if !command_name(name) || offered.iter().filter(|entry| entry.name == name).count() != 1 {
        return Err(AgentError::Unsupported(
            "ACP command is not offered by this session".into(),
        ));
    }
    if arguments.len() > MAX_PROMPT_BYTES || arguments.contains('\0') {
        return Err(invalid(
            "ACP command arguments exceed their budget or contain NUL",
        ));
    }
    Ok(if arguments.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {arguments}")
    })
}

pub(crate) fn supports_operation(capabilities: &Value, operation: &str) -> bool {
    capabilities["sessionCapabilities"][operation].is_object()
}

pub(crate) fn delete_params(id: &str) -> Result<Value, AgentError> {
    if id.is_empty() {
        return Err(invalid("ACP session identity is empty"));
    }
    Ok(json!({"sessionId":id}))
}

pub(crate) fn forked_session_id(source: &str, response: &Value) -> Result<String, AgentError> {
    response["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty() && *id != source)
        .map(str::to_owned)
        .ok_or_else(|| {
            AgentError::Protocol(
                "ACP fork returned no distinct session identity; do not retry creation".into(),
            )
        })
}

pub(crate) fn fork_params(id: &str, cwd: &str, message: Option<&str>) -> Result<Value, AgentError> {
    if message.is_some() {
        return Err(AgentError::Unsupported(
            "ACP fork at a message boundary".into(),
        ));
    }
    if id.is_empty() || !Path::new(cwd).is_absolute() {
        return Err(invalid(
            "ACP fork requires a session identity and absolute workspace",
        ));
    }
    Ok(json!({"sessionId":id,"cwd":cwd,"mcpServers":[]}))
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
    fn scratch() -> Scratch {
        let path = std::env::temp_dir().join(format!("muqun-acp-uploads-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
    fn caps() -> PromptCapabilities {
        PromptCapabilities {
            images: true,
            embedded_context: true,
        }
    }
    fn png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=").unwrap()
    }
    #[tokio::test]
    async fn pure_text_and_json_escaping_share_the_native_prompt_budget() {
        let plain = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(prompt_parts(&plain, &[], caps()).await.is_err());
        let escaped = "\"".repeat(MAX_PROMPT_BYTES / 2 + 1);
        assert!(prompt_parts(&escaped, &[], caps()).await.is_err());
        let accepted = prompt_parts("hello", &[], PromptCapabilities::default())
            .await
            .unwrap();
        assert_eq!(
            accepted.content,
            vec![json!({"type":"text","text":"hello"})]
        );
        assert_eq!(accepted.transcript, "hello");
    }

    #[test]
    fn uploaded_images_and_text_are_real_native_parts_and_retain_original_paths() {
        let root = scratch();
        let image = root.0.join("renamed.txt");
        let note = root.0.join("note.md");
        std::fs::write(&image, png()).unwrap();
        std::fs::write(&note, "hello\n世界").unwrap();
        let paths = vec![
            image.to_string_lossy().into_owned(),
            note.to_string_lossy().into_owned(),
        ];
        let prompt = upload_parts("inspect", &paths, &root.0, caps()).unwrap();
        assert_eq!(prompt.content[1]["type"], "image");
        assert_eq!(prompt.content[1]["mimeType"], "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(prompt.content[1]["data"].as_str().unwrap())
                .unwrap(),
            png()
        );
        assert_eq!(prompt.content[2]["resource"]["text"], "hello\n世界");
        assert!(prompt.transcript.contains(&paths[0]));
        assert!(prompt.transcript.contains(&paths[1]));
    }
    #[test]
    fn uploads_fence_refuses_remote_data_outside_paths_and_escaping_symlinks() {
        let root = scratch();
        let uploads = root.0.join("uploads");
        std::fs::create_dir(&uploads).unwrap();
        let outside = root.0.join("private.txt");
        std::fs::write(&outside, "private").unwrap();
        let mut denied = vec![
            "https://example.test/a.png".into(),
            "data:image/png;base64,AAA=".into(),
            outside.to_string_lossy().into_owned(),
            "relative.txt".into(),
        ];
        #[cfg(unix)]
        {
            let link = uploads.join("link");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            denied.push(link.to_string_lossy().into_owned());
        }
        for path in denied {
            assert!(upload_parts("", &[path], &uploads, caps()).is_err());
        }
    }
    #[test]
    fn missing_image_capability_and_binary_resources_are_refused() {
        let root = scratch();
        let image = root.0.join("image.png");
        std::fs::write(&image, png()).unwrap();
        assert!(upload_parts(
            "",
            &[image.to_string_lossy().into_owned()],
            &root.0,
            PromptCapabilities::default()
        )
        .is_err());
        for (name, bytes) in [
            ("data.pdf", b"%PDF-1.7\nexample".as_slice()),
            ("data.bin", b"binary\0body".as_slice()),
        ] {
            let path = root.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            assert!(
                upload_parts("", &[path.to_string_lossy().into_owned()], &root.0, caps()).is_err()
            );
        }
    }
    #[test]
    fn encoded_image_and_whole_prompt_limits_are_enforced_before_submission() {
        let root = scratch();
        let path = root.0.join("large.png");
        let mut bytes = png();
        bytes.resize(MAX_IMAGE_BYTES + 1, 0);
        std::fs::write(&path, bytes).unwrap();
        assert!(upload_parts("", &[path.to_string_lossy().into_owned()], &root.0, caps()).is_err());
        let note = root.0.join("escaped.txt");
        std::fs::write(&note, "\"".repeat(MAX_PROMPT_BYTES / 2 + 1)).unwrap();
        assert!(upload_parts("", &[note.to_string_lossy().into_owned()], &root.0, caps()).is_err());
    }
    #[test]
    fn discovered_commands_are_exact_and_repeated_names_listed_once() {
        let offered=commands(&json!({"availableCommands":[{"name":"rename","description":"Rename","input":{"hint":"title"}},{"name":"plugin:task"}]})).unwrap();
        assert_eq!(
            command_prompt(&offered, "rename", "New title").unwrap(),
            "/rename New title"
        );
        assert_eq!(offered[0].template.as_deref(), Some("title"));
        assert!(command_prompt(&offered, "unknown", "").is_err());
        assert!(command_prompt(&offered, "rename\nrewind", "").is_err());
        let repeated = commands(&json!({"availableCommands":[{"name":"same","description":"global"},{"name":"same","description":"skill"}]})).unwrap();
        assert_eq!(repeated.len(), 1);
        assert_eq!(repeated[0].description.as_deref(), Some("global"));
        assert_eq!(command_prompt(&repeated, "same", "").unwrap(), "/same");
        assert!(commands(&json!({"availableCommands":[{"name":"bad name"}]})).is_err());
        assert!(commands(&json!({"availableCommands":[]}))
            .unwrap()
            .is_empty());
    }
    #[test]
    fn whole_session_fork_never_silently_accepts_a_message_cutoff() {
        assert_eq!(
            fork_params("opaque", "/workspace", None).unwrap(),
            json!({"sessionId":"opaque","cwd":"/workspace","mcpServers":[]})
        );
        assert!(fork_params("opaque", "/workspace", Some("message")).is_err());
        assert!(fork_params("opaque", "/workspace", Some("")).is_err());
    }
    #[test]
    fn session_operations_require_advertised_capabilities_and_distinct_fork_identity() {
        assert!(supports_operation(
            &json!({"sessionCapabilities":{"fork":{},"delete":{}}}),
            "fork"
        ));
        assert!(!supports_operation(
            &json!({"sessionCapabilities":{"fork":false}}),
            "fork"
        ));
        assert!(!supports_operation(&json!({}), "delete"));
        assert!(delete_params("").is_err());
        assert_eq!(
            delete_params("opaque").unwrap(),
            json!({"sessionId":"opaque"})
        );
        for response in [
            json!({}),
            json!({"sessionId":""}),
            json!({"sessionId":"source"}),
        ] {
            assert!(forked_session_id("source", &response).is_err());
        }
        assert_eq!(
            forked_session_id("source", &json!({"sessionId":"new"})).unwrap(),
            "new"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn late_command_catalog_reaches_existing_ui_and_submits_exactly_once() {
        use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};
        use crate::agents::adapters::memory_mirror::MemoryMirror;
        use crate::agents::domain::{AgentDomainEvent, AgentSessionStatus};
        use crate::agents::ports::agent::AgentPort;
        use std::sync::Arc;
        use std::time::Duration;

        struct ScriptProvider;
        impl AcpProvider for ScriptProvider {
            const KIND: &'static str = "qoder";
            const LABEL: &'static str = "Qoder command fixture";
            const REQUIRE_MESSAGE_ID: bool = false;
            const MODEL_SELECTION: bool = false;
            const ARGS: &'static [&'static str] = &[
                "-u",
                "-c",
                r#"
import json, os, time, sys
sid='native-operations-session'
def send(v): print(json.dumps(v),flush=True)
def reply(rid,v): send({'jsonrpc':'2.0','id':rid,'result':v})
def update(v): send({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':sid,'update':v}})
for line in sys.stdin:
    value=json.loads(line);method=value.get('method');rid=value.get('id')
    if method=='initialize': reply(rid,{'protocolVersion':1,'agentCapabilities':{'loadSession':True,'sessionCapabilities':{'list':{}},'promptCapabilities':{'image':True,'embeddedContext':True}}})
    elif method=='session/new':
        reply(rid,{'sessionId':sid,'configOptions':[]})
        while not os.path.exists('publish-commands'): time.sleep(0.005)
        update({'sessionUpdate':'available_commands_update','availableCommands':[{'name':'rename','input':{'hint':'title'}}]})
    elif method=='session/prompt':
        with open('received-prompts.jsonl','a') as output: output.write(json.dumps(value)+'\n')
        update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'done'}})
        reply(rid,{'stopReason':'end_turn'})
"#,
            ];
        }
        let root = scratch();
        let (tx, mut events) = tokio::sync::broadcast::channel(256);
        let driver = AcpDriver::<ScriptProvider>::new(
            "python3".into(),
            root.0.to_string_lossy().into_owned(),
            Arc::new(MemoryMirror::for_agent("qoder")),
            tx,
        );
        let info = driver.create_session(None, None, None).await.unwrap();
        std::fs::write(root.0.join("publish-commands"), b"ready").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(AgentDomainEvent::Resync { reason, .. }) = events.recv().await {
                    if reason == "agent_catalog_changed" {
                        break;
                    }
                }
            }
        })
        .await
        .expect("late native catalog must refresh the existing App catalog");
        assert!(driver.slash_commands_listed());
        let catalog = driver
            .get_catalog_for_session(Some(&info.asid.0), None)
            .await
            .unwrap();
        assert_eq!(catalog.commands[0].name, "rename");
        driver
            .run_command(&info.asid.0, "rename", "new title", None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if driver.get_session(&info.asid.0).await.unwrap().status
                    == AgentSessionStatus::Idle
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(driver
            .run_command(&info.asid.0, "not-offered", "", None)
            .await
            .is_err());
        let received = std::fs::read_to_string(root.0.join("received-prompts.jsonl")).unwrap();
        let records: Vec<_> = received
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0]["params"]["prompt"][0]["text"],
            "/rename new title"
        );
        driver.shutdown().await;
    }
}
