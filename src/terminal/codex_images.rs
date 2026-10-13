//! Images from the conversation the terminal backend explicitly identifies.
//! No cwd/newest-file discovery, raw-path API, or inferred conversation binding.
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::agents::attachment_images::{image_uri, MAX_RESPONSE_IMAGE_BYTES};
use crate::terminal::backend::{Agent, PaneId};
use crate::{terminal_backend, SessionConfig};

const MAX_TAIL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MESSAGES: usize = 64;

#[derive(Clone)]
struct UserImages {
    text: String,
    sources: Vec<String>,
}

fn binding(agents: Vec<Agent>, pane: &str) -> Option<(String, String)> {
    let mut matching = agents
        .into_iter()
        .filter(|agent| agent.pane_id == PaneId::new(pane));
    let agent = matching.next()?;
    if matching.next().is_some()
        || !agent
            .kind
            .as_deref()?
            .to_ascii_lowercase()
            .contains("codex")
    {
        return None;
    }
    let conversation = agent.conversation_id?;
    // A filename suffix, not a caller-controlled path. Preserve exact spelling.
    let parsed = uuid::Uuid::parse_str(&conversation).ok()?;
    if parsed.hyphenated().to_string() != conversation {
        return None;
    }
    Some((agent.instance_id?, conversation))
}

/// Sample before reading the terminal text, then compare again after image I/O.
pub(crate) async fn snapshot_binding(
    session: &SessionConfig,
    pane: &str,
) -> Option<(String, String)> {
    binding(terminal_backend(session).list_agents().await.ok()?, pane)
}

pub(crate) async fn attach(
    session: &SessionConfig,
    pane: &str,
    before: Option<(String, String)>,
    parts: &mut [Value],
) {
    if !parts.iter().any(|part| {
        part["type"] == "prompt"
            && part["text"]
                .as_str()
                .is_some_and(|text| text.contains("[Image #"))
    }) {
        return;
    }
    let Some(before) = before else {
        return;
    };
    let Some(home) = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
    else {
        return;
    };
    let id = before.1.clone();
    let mut enriched = parts.to_vec();
    let Ok(Some(enriched)) = tokio::task::spawn_blocking(move || {
        let file = find_rollout(&home.join("sessions"), &id)?;
        let messages = read_rollout(&file, &id)?;
        enrich(&mut enriched, &messages);
        Some(enriched)
    })
    .await
    else {
        return;
    };
    // A pane can be replaced while reading a large transcript. Never return
    // images sampled under a previous process/conversation incarnation.
    if snapshot_binding(session, pane).await.as_ref() == Some(&before) {
        parts.clone_from_slice(&enriched);
    }
}

fn find_rollout(root: &Path, id: &str) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let suffix = format!("-{id}.jsonl");
    let mut directories = vec![(root, 0)];
    let mut examined = 0;
    let mut found = None;
    while let Some((directory, depth)) = directories.pop() {
        for entry in std::fs::read_dir(directory).ok()? {
            examined += 1;
            if examined > 30_000 {
                return None;
            }
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            if kind.is_dir() && depth < 3 {
                directories.push((entry.path(), depth + 1));
            }
            if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
            {
                if found.is_some() {
                    return None;
                }
                found = Some(entry.path());
            }
        }
    }
    found
}

fn read_rollout(path: &Path, id: &str) -> Option<Vec<UserImages>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut header = Vec::new();
    BufReader::new((&mut file).take(256 * 1024))
        .read_until(b'\n', &mut header)
        .ok()?;
    let header: Value = serde_json::from_slice(&header).ok()?;
    if header["type"] != "session_meta" || header["payload"]["id"] != id {
        return None;
    }
    let offset = metadata.len().saturating_sub(MAX_TAIL_BYTES);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let bytes = if offset > 0 {
        &bytes[bytes.iter().position(|b| *b == b'\n')? + 1..]
    } else {
        &bytes
    };
    parse_messages(bytes)
}

fn strings(content: &Value, kind: &str, field: &str) -> Vec<String> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == kind)
        .filter_map(|part| part[field].as_str())
        .map(str::to_owned)
        .collect()
}

fn parse_messages(bytes: &[u8]) -> Option<Vec<UserImages>> {
    let mut messages = VecDeque::new();
    let mut pending: Option<(String, Vec<String>)> = None;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        // A writer can still be appending its last record. Never parse it yet.
        if line.last() != Some(&b'\n') {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            pending = None;
            continue;
        }
        let Ok(row) = serde_json::from_slice::<Value>(line) else {
            pending = None;
            continue;
        };
        let payload = &row["payload"];
        if row["type"] == "response_item"
            && payload["type"] == "message"
            && payload["role"] == "user"
        {
            let text = strings(&payload["content"], "input_text", "text")
                .pop()
                .unwrap_or_default();
            let sources = strings(&payload["content"], "input_image", "image_url");
            pending = Some((text, sources));
            continue;
        }
        let user = if row["type"] == "event_msg"
            && payload["type"] == "item_completed"
            && payload["item"]["type"] == "UserMessage"
        {
            let item = &payload["item"];
            Some(UserImages {
                text: strings(&item["content"], "text", "text").join("\n"),
                sources: item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| match part["type"].as_str()? {
                        "local_image" => part["path"].as_str(),
                        "image" => part["image_url"].as_str().or_else(|| part["url"].as_str()),
                        _ => None,
                    })
                    .take(16)
                    .map(str::to_owned)
                    .collect(),
            })
        } else if row["type"] == "event_msg" && payload["type"] == "user_message" {
            let sources = ["images", "local_images"]
                .into_iter()
                .flat_map(|key| payload[key].as_array().into_iter().flatten())
                .filter_map(Value::as_str)
                .take(16)
                .map(str::to_owned)
                .collect();
            Some(UserImages {
                text: payload["message"].as_str().unwrap_or_default().into(),
                sources,
            })
        } else {
            None
        };
        if let Some(mut user) = user {
            // The producer writes the encoded response message before its
            // typed user event. Both text and image count must agree before
            // using those durable bytes instead of a possibly-deleted file.
            if let Some((text, images)) = pending.take() {
                if text == user.text && images.len() == user.sources.len() && !images.is_empty() {
                    user.sources = images;
                }
            }
            messages.push_back(user);
            if messages.len() > MAX_MESSAGES {
                messages.pop_front();
            }
        }
    }
    Some(messages.into())
}

fn prompt_text(text: &str) -> String {
    // Codex draws numbered image labels before the user's own text. Labels
    // have no authority to choose a file; only the typed transcript does.
    let mut rest = text.trim();
    while let Some(after) = rest.strip_prefix("[Image #") {
        let Some((number, suffix)) = after.split_once(']') else {
            break;
        };
        if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
            break;
        }
        rest = suffix.trim_start();
    }
    rest.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn enrich(parts: &mut [Value], messages: &[UserImages]) {
    let prompts: Vec<_> = parts
        .iter()
        .enumerate()
        .filter(|(_, part)| part["type"] == "prompt")
        .map(|(i, part)| (i, prompt_text(part["text"].as_str().unwrap_or_default())))
        .collect();
    let mut matches = Vec::new();
    for (index, text) in &prompts {
        if prompts.iter().filter(|(_, other)| other == text).count() != 1 {
            continue;
        }
        let candidates: Vec<_> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| prompt_text(&message.text) == *text)
            .collect();
        let [(source_index, message)] = candidates.as_slice() else {
            continue;
        };
        let mut rest = parts[*index]["text"].as_str().unwrap_or_default().trim();
        let mut labels = 0;
        while let Some(after) = rest.strip_prefix("[Image #") {
            let Some((number, suffix)) = after.split_once(']') else {
                break;
            };
            if number.parse::<usize>().ok() != Some(labels + 1) {
                break;
            }
            labels += 1;
            rest = suffix.trim_start();
        }
        if labels == 0 || labels != message.sources.len() {
            continue;
        }
        matches.push((*index, *source_index, *message));
    }
    // The exact producer conversation is already bound and header-verified.
    // Match only unique, complete prompt text + numbered image count, with
    // monotonically increasing producer order. Never use substring matches,
    // directory proximity, or a latest-file heuristic. Unsubmitted composer
    // rows and terminal footers are left alone, as are ambiguous repeats.
    if matches.windows(2).any(|pair| pair[0].1 >= pair[1].1) {
        return;
    }
    let mut budget = MAX_RESPONSE_IMAGE_BYTES;
    for (index, _, message) in matches {
        let sources: Vec<_> = message
            .sources
            .iter()
            .take(16)
            .filter_map(|source| image_uri(source, &mut budget))
            .collect();
        if !sources.is_empty() {
            parts[index]["attachments"] = serde_json::json!(sources);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const ID: &str = "019a0000-0000-7000-8000-000000000001";
    const OTHER: &str = "019a0000-0000-7000-8000-000000000002";
    const IMAGE: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=";

    fn record(value: Value) -> String {
        format!("{value}\n")
    }
    fn user(text: &str) -> String {
        record(
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                {"type":"input_image","image_url":IMAGE},{"type":"input_text","text":text}
            ]}}),
        ) + &record(
            json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"synthetic-user","content":[
                {"type":"local_image","path":"/missing/synthetic-clipboard.png"},{"type":"text","text":text,"text_elements":[]}
            ]}}}),
        )
    }
    fn prompt(text: &str) -> Value {
        json!({"type":"prompt","text":text,"fallback_text":text,"range":{"start":0,"end":0}})
    }

    #[test]
    fn typed_rollout_pairs_encoded_image_and_enriches_ordered_prompt_suffix() {
        let source = record(
            json!({"type":"event_msg","payload":{"type":"user_message","message":"Earlier","images":[],"local_images":[]}}),
        ) + &user("[Image #1] Inspect icon");
        let messages = parse_messages(source.as_bytes()).unwrap();
        assert_eq!(messages[1].sources, [IMAGE]);
        let mut parts = vec![prompt("Earlier"), prompt("[Image #1] Inspect icon")];
        enrich(&mut parts, &messages);
        assert!(parts[0].get("attachments").is_none());
        assert_eq!(parts[1]["attachments"], json!([IMAGE]));
        assert_eq!(parts[1]["text"], "[Image #1] Inspect icon");
        assert_eq!(parts[1]["range"], json!({"start":0,"end":0}));
    }

    #[test]
    fn duplicates_latest_mismatch_and_untyped_paths_cannot_select_images() {
        let messages = parse_messages((user("Inspect") + &user("Inspect")).as_bytes()).unwrap();
        let mut parts = vec![prompt("[Image #1] Inspect")];
        enrich(&mut parts, &messages);
        assert!(parts[0].get("attachments").is_none());
        let messages = parse_messages(user("Other turn").as_bytes()).unwrap();
        enrich(&mut parts, &messages);
        assert!(parts[0].get("attachments").is_none());
        let messages = parse_messages(record(json!({"type":"event_msg","payload":{"type":"user_message","message":"Inspect /host/photo.png","images":[],"local_images":[]}})).as_bytes()).unwrap();
        let mut parts = vec![prompt("[Image #1] Inspect /host/photo.png")];
        enrich(&mut parts, &messages);
        assert!(parts[0].get("attachments").is_none());
    }

    #[test]
    fn rollout_discovery_and_header_require_the_exact_conversation() {
        let root = std::env::temp_dir().join(format!("gateway-rollout-{}", uuid::Uuid::new_v4()));
        let day = root.join("2026/01/01");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-synthetic-{ID}.jsonl"));
        std::fs::write(
            &path,
            record(json!({"type":"session_meta","payload":{"id":OTHER}})) + &user("Inspect"),
        )
        .unwrap();
        assert_eq!(find_rollout(&root, ID), Some(path.clone()));
        assert!(read_rollout(&path, ID).is_none());
        std::fs::write(
            &path,
            record(json!({"type":"session_meta","payload":{"id":ID}})) + &user("Inspect"),
        )
        .unwrap();
        assert_eq!(read_rollout(&path, ID).unwrap()[0].sources, [IMAGE]);
        std::fs::write(day.join(format!("rollout-second-{ID}.jsonl")), "").unwrap();
        assert!(find_rollout(&root, ID).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unfinished_and_oversized_records_do_not_produce_images() {
        let source = user("Inspect");
        let messages = parse_messages(source.trim_end().as_bytes()).unwrap();
        assert!(messages.is_empty());
        let source = format!("{}\n", "x".repeat(MAX_LINE_BYTES + 1)) + &user("Inspect");
        assert_eq!(parse_messages(source.as_bytes()).unwrap().len(), 1);
    }

    #[test]
    fn image_count_and_producer_order_must_agree() {
        let messages = parse_messages((user("First") + &user("Second")).as_bytes()).unwrap();
        let mut parts = vec![prompt("[Image #1] [Image #2] First")];
        enrich(&mut parts, &messages);
        assert!(parts[0].get("attachments").is_none());
        let mut parts = vec![prompt("[Image #1] Second"), prompt("[Image #1] First")];
        enrich(&mut parts, &messages);
        assert!(parts.iter().all(|part| part.get("attachments").is_none()));
        let mut parts = vec![
            prompt("[Image #1] First"),
            prompt("Unsubmitted composer draft"),
        ];
        enrich(&mut parts, &messages);
        assert_eq!(parts[0]["attachments"], json!([IMAGE]));
        assert!(parts[1].get("attachments").is_none());
    }

    #[test]
    fn binding_requires_one_codex_instance_and_a_canonical_conversation_id() {
        let agent = Agent {
            conversation_id: Some(ID.into()),
            instance_id: Some("incarnation".into()),
            target: "target".into(),
            pane_id: PaneId::new("pane"),
            workspace_id: None,
            tab_id: None,
            kind: Some("codex".into()),
            display_agent: None,
            status: crate::terminal::backend::AgentStatus::Idle,
            state_change_seq: None,
            shell_pid: None,
            pending_user_input: None,
        };
        let before = binding(vec![agent.clone()], "pane").unwrap();
        assert!(binding(vec![agent.clone(), agent.clone()], "pane").is_none());
        assert!(binding(vec![agent.clone()], "another-pane").is_none());
        let mut changed = agent.clone();
        changed.instance_id = Some("replacement".into());
        assert_ne!(binding(vec![changed], "pane").unwrap(), before);
        let mut changed = agent.clone();
        changed.conversation_id = Some(OTHER.into());
        assert_ne!(binding(vec![changed], "pane").unwrap(), before);
        let mut changed = agent.clone();
        changed.conversation_id = Some("../conversation".into());
        assert!(binding(vec![changed], "pane").is_none());
        let mut changed = agent;
        changed.kind = Some("claude".into());
        assert!(binding(vec![changed], "pane").is_none());
    }

    #[test]
    #[ignore = "read-only diagnostic: requires explicit rollout id, Codex home and terminal text file"]
    fn codex_images_read_only_probe() {
        let id = std::env::var("MUQUN_CODEX_IMAGE_PROBE_ID").unwrap();
        let home = std::env::var("MUQUN_CODEX_IMAGE_PROBE_HOME").unwrap();
        let screen =
            std::fs::read_to_string(std::env::var("MUQUN_CODEX_IMAGE_PROBE_SCREEN").unwrap())
                .unwrap();
        let file = find_rollout(&PathBuf::from(home).join("sessions"), &id).unwrap();
        let messages = read_rollout(&file, &id).unwrap();
        let mut parts =
            crate::parts::normalize_json(&screen, crate::parts::dictionary_for(Some("codex")));
        enrich(&mut parts, &messages);
        let image_count: usize = parts
            .iter()
            .filter_map(|p| p["attachments"].as_array())
            .map(Vec::len)
            .sum();
        eprintln!(
            "source_user_messages={} terminal_prompts={} exported_images={}",
            messages.len(),
            parts.iter().filter(|p| p["type"] == "prompt").count(),
            image_count
        );
        assert!(
            image_count > 0,
            "no images aligned with the bounded terminal snapshot"
        );
    }
}
