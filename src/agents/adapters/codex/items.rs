//! Pure projections of the pinned app-server ThreadItem union.
use crate::agents::domain::*;
use serde_json::{json, Value};

fn present(value: &Value, key: &str) -> Option<Value> {
    value.get(key).filter(|v| !v.is_null()).cloned()
}
fn state(value: &Value) -> ToolCallStatus {
    match value["status"].as_str() {
        Some("completed") => ToolCallStatus::Completed,
        Some("failed" | "declined" | "interrupted") => ToolCallStatus::Failed,
        _ => ToolCallStatus::Running,
    }
}
fn tool(value: &Value, name: String, input: Value, output: Option<Value>) -> ToolCall {
    let mut metadata = value.clone();
    if let Some(metadata) = metadata.as_object_mut() {
        for key in [
            "aggregatedOutput",
            "arguments",
            "result",
            "changes",
            "contentItems",
            "agentsStates",
            "receiverThreadIds",
        ] {
            metadata.remove(key);
        }
    }
    let state = state(value);
    ToolCall {
        id: value["id"].as_str().unwrap_or_default().into(),
        name,
        title: None,
        input,
        output,
        content: None,
        metadata: Some(metadata),
        state,
        status: state,
        error: value["error"]["message"]
            .as_str()
            .map(|message| AgentErrorInfo {
                name: "CodexToolError".into(),
                message: message.into(),
                status: None,
            }),
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated: false,
        time: ToolTime::default(),
    }
}

fn patch_counts(diff: &str) -> Option<(usize, usize)> {
    let mut in_hunk = false;
    let mut saw_hunk = false;
    let (mut added, mut removed) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
            saw_hunk = true;
            continue;
        }
        if in_hunk {
            if line.starts_with('+') {
                added += 1;
            }
            if line.starts_with('-') {
                removed += 1;
            }
        }
    }
    saw_hunk.then_some((added, removed))
}

/// The file Codex wrote, not the base64 `result` the same item also carries.
///
/// Official `savedPath` is an absolute host path; `result` is base64 PNG.
/// Retain the older prose-file-URI fallback without treating it as a raw path.
fn saved_file(value: &Value) -> Option<reqwest::Url> {
    let direct = value
        .get("savedPath")
        .or_else(|| value.get("saved_path"))
        .and_then(Value::as_str)
        .filter(|path| hosted_path(path));
    if let Some(path) = direct {
        return reqwest::Url::from_file_path(path).ok();
    }
    let text = value.get("result").and_then(Value::as_str)?;
    let rest = text.split("file://").nth(1)?;
    let mut parens = 0usize;
    let mut end = rest.len();
    for (at, c) in rest.char_indices() {
        if c.is_whitespace() || matches!(c, ']' | '"' | '\'' | '>') || (c == ')' && parens == 0) {
            end = at;
            break;
        }
        match c {
            '(' => parens += 1,
            ')' => parens -= 1,
            _ => {}
        }
    }
    let uri = reqwest::Url::parse(&format!("file://{}", &rest[..end])).ok()?;
    if uri.query().is_some() || uri.fragment().is_some() {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    hosted_path(path.to_str()?)
        .then(|| reqwest::Url::from_file_path(path).ok())
        .flatten()
}

fn hosted_path(path: &str) -> bool {
    !path.is_empty()
        && path.starts_with('/')
        && !path.starts_with("//")
        && !path.chars().any(char::is_control)
        && path.len() <= 4096
}

fn image_generation(message: &str, id: &str, value: &Value) -> Vec<(String, AgentPart)> {
    let mut source = value.clone();
    if let Some(object) = source.as_object_mut() {
        object.remove("result");
    }
    let prompt = value["revisedPrompt"]
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or("");
    let mut call = tool(
        &source,
        "image_generation".into(),
        json!({ "prompt": prompt }),
        None,
    );
    if value.get("status").is_none() {
        call.state = ToolCallStatus::Completed;
        call.status = call.state;
    }
    let saved = saved_file(value);
    if saved.is_none() {
        if let Some(result) = value["result"].as_str().filter(|result| !result.is_empty()) {
            // Save failure is valid upstream. Reuse the bounded, MIME-checked
            // producer-image helper and existing typed tool content, not raw output.
            let mut budget = crate::agents::attachment_images::MAX_RESPONSE_IMAGE_BYTES;
            let image = (result.len()
                <= crate::agents::attachment_images::MAX_IMAGE_BYTES.div_ceil(3) * 4)
                .then(|| format!("data:image/png;base64,{result}"))
                .and_then(|source| {
                    crate::agents::attachment_images::image_uri(&source, &mut budget)
                })
                .filter(|uri| uri.starts_with("data:image/png;"));
            call.content = Some(match image {
                Some(uri) => json!([{"type":"file", "uri":uri, "mime":"image/png"}]),
                None => {
                    json!([{"type":"text", "text":"Generated image is unavailable: no usable saved path or supported inline image."}])
                }
            });
        }
    }
    let mut parts = vec![(tool_item_id(message, id), AgentPart::Tool(call))];
    if let Some(uri) = saved {
        parts.push((
            part_item_id(message, 1),
            AgentPart::Text {
                text: format!("![](<{uri}>)"),
            },
        ));
    }
    parts
}

pub(super) fn parts(message: &str, id: &str, value: &Value) -> Vec<(String, AgentPart)> {
    let kind = value["type"].as_str().unwrap_or_default();
    if kind == "collabAgentToolCall" || kind == "subAgentActivity" {
        return collaboration(message, id, value);
    }
    if kind == "imageGeneration" {
        return image_generation(message, id, value);
    }
    let mut call = match kind {
        "commandExecution" => {
            let mut call = tool(
                value,
                "shell".into(),
                json!({"command":value["command"],"cwd":value["cwd"]}),
                present(value, "aggregatedOutput"),
            );
            if let Some(exit) = value["exitCode"].as_i64() {
                call.metadata.as_mut().unwrap()["exit"] = json!(exit);
            }
            // A PTY processId identifies a process, not proof of detachment.
            call
        }
        "fileChange" => {
            let mut call = tool(
                value,
                "edit".into(),
                json!({"paths":value["changes"].as_array().into_iter().flatten().filter_map(|change| change["path"].as_str()).collect::<Vec<_>>()}),
                None,
            );
            let files: Vec<_> = value["changes"].as_array().into_iter().flatten().map(|change| {
                let diff = change["diff"].as_str().unwrap_or_default();
                let mut file = json!({"path":change["path"],"diff":change["diff"],"status":change["kind"]["type"]});
                // Counts derive only from the supplied unified patch, not the worktree.
                if let Some((added, removed)) = patch_counts(diff) {
                    file["additions"] = json!(added);
                    file["deletions"] = json!(removed);
                }
                if let Some(path) = change["kind"]["move_path"].as_str() { file["move_path"] = json!(path); }
                file
            }).collect();
            call.metadata.as_mut().unwrap()["files"] = json!(files);
            call
        }
        "mcpToolCall" => tool(
            value,
            format!(
                "{}:{}",
                value["server"].as_str().unwrap_or_default(),
                value["tool"].as_str().unwrap_or_default()
            ),
            value["arguments"].clone(),
            present(value, "result"),
        ),
        "dynamicToolCall" => {
            let name = match value["namespace"].as_str() {
                Some(namespace) => {
                    format!("{namespace}:{}", value["tool"].as_str().unwrap_or_default())
                }
                None => value["tool"].as_str().unwrap_or_default().into(),
            };
            tool(
                value,
                name,
                value["arguments"].clone(),
                present(value, "contentItems"),
            )
        }
        "webSearch" => tool(
            value,
            "web_search".into(),
            json!({"query":value["query"],"action":value["action"]}),
            present(value, "results"),
        ),
        "sleep" => {
            let mut call = tool(
                value,
                "sleep".into(),
                json!({"durationMs":value["durationMs"]}),
                None,
            );
            // Requested sleep time is not a measured execution duration.
            call.metadata
                .as_mut()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("durationMs");
            call
        }
        "imageView" => tool(
            value,
            "image_view".into(),
            json!({"path":value["path"]}),
            None,
        ),
        "functionCallOutput" => tool(
            value,
            value["name"].as_str().unwrap_or("tool").into(),
            Value::Null,
            present(value, "output"),
        ),
        "enteredReviewMode" | "exitedReviewMode" => {
            return vec![(
                part_item_id(message, 0),
                AgentPart::Status {
                    text: value["review"].as_str().unwrap_or_default().into(),
                },
            )]
        }
        "contextCompaction" => {
            return vec![(
                part_item_id(message, 0),
                AgentPart::Compaction {
                    status: if value["status"] == "inProgress" {
                        CompactionStatus::Running
                    } else {
                        CompactionStatus::Completed
                    },
                    reason: None,
                    summary: None,
                    recent: None,
                    tokens: None,
                    cost: None,
                    error: None,
                },
            )]
        }
        _ => return vec![],
    };
    // Canonical history items without a lifecycle field are complete. Live
    // item/started/completed supplies the envelope lifecycle before projection.
    if value.get("status").is_none() {
        call.state = ToolCallStatus::Completed;
        call.status = call.state;
    }
    vec![(tool_item_id(message, id), AgentPart::Tool(call))]
}

fn collaboration(message: &str, id: &str, value: &Value) -> Vec<(String, AgentPart)> {
    let activity = value["type"] == "subAgentActivity";
    let targets: Vec<&str> = if activity {
        value["agentThreadId"].as_str().into_iter().collect()
    } else {
        value["receiverThreadIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect()
    };
    let mut targets = targets;
    targets.sort_unstable();
    targets.dedup();
    // A failed spawn or list operation can have no receiver; retain the call.
    let targets: Vec<Option<&str>> = if targets.is_empty() {
        vec![None]
    } else {
        targets.into_iter().map(Some).collect()
    };
    targets.into_iter().map(|target| {
        let mut call = tool(value,"subagent".into(),json!({"prompt":value["prompt"],"operation":value["tool"],"model":value["model"],"reasoningEffort":value["reasoningEffort"]}),None);
        call.child_session_id = target.map(str::to_owned);
        let metadata = call.metadata.as_mut().unwrap();
        metadata.as_object_mut().unwrap().remove("status");
        if !activity { metadata["collabStatus"] = value["status"].clone(); }
        let child_status = if activity {
            match value["kind"].as_str() {
                Some("started") => Some("running"),
                Some("completed") => Some("completed"),
                Some("interrupted") => Some("interrupted"),
                _ => None,
            }
        } else { target.and_then(|target| value["agentsStates"][target]["status"].as_str()) };
        if let Some(status) = child_status {
            metadata["status"] = json!(match status { "pendingInit" => "starting", "running" => "working", "errored" => "failed", other => other });
        }
        if let Some(target) = target {
            metadata["sessionID"] = json!(target);
            call.output = present(&value["agentsStates"][target],"message");
        }
        if activity {
            call.state = ToolCallStatus::Completed;
            call.status = call.state;
            call.title = value["agentPath"].as_str().map(str::to_owned);
        }
        let row_id = target.map(|target| format!("{}:child:{}:{target}",tool_item_id(message,id),target.len())).unwrap_or_else(|| tool_item_id(message,id));
        (row_id,AgentPart::Tool(call))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::codex::mapper;

    fn mapped(value: Value) -> Vec<ToolCall> {
        mapper::item("thread", "turn", &value, 0)
            .into_iter()
            .map(|row| {
                let AgentPart::Tool(call) = row.part else {
                    panic!("expected tool")
                };
                call
            })
            .collect()
    }

    struct ImageWorkspace(std::path::PathBuf);

    impl ImageWorkspace {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("codex-image-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("generated_images")).unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }

        fn image(&self, name: &str) -> std::path::PathBuf {
            use base64::Engine as _;
            let path = self.0.join("generated_images").join(name);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(TEST_IMAGE)
                .unwrap();
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }

    impl Drop for ImageWorkspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Synthetic 1x1 test PNG, not a user's generated image or transcript.
    const TEST_IMAGE: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6qlsAAAAASUVORK5CYII=";

    async fn assert_image_pipeline(
        workspace: &ImageWorkspace,
        path: &std::path::Path,
        value: Value,
    ) {
        use crate::agents::message_images::attach_to_items;
        use crate::platform::assets::asset_download;
        use axum::extract::{Path as RoutePath, State};
        use axum::http::{HeaderMap, StatusCode};

        let state = crate::test_state("admin", vec![crate::test_device("d1", "token")]);
        let mut rows = mapper::item("thread", "turn", &value, 0);
        let call = rows
            .iter()
            .find_map(|row| match &row.part {
                AgentPart::Tool(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert_eq!(call.input["prompt"], "a test image");
        assert!(call.output.is_none());
        assert!(call.metadata.as_ref().unwrap().get("result").is_none());
        attach_to_items(&state, "thread", workspace.0.to_str(), &mut rows).await;
        let assets: Vec<_> = rows
            .iter()
            .flat_map(|row| row.image_assets.iter().flatten())
            .collect();
        assert!(
            !assets.is_empty(),
            "mapper image did not resolve: {}",
            path.display()
        );
        let id = crate::asset_id(path);
        assert!(assets
            .iter()
            .all(|asset| asset.asset_id == id && asset.mime == "image/png"));
        let served = state.message_images.served(&id).unwrap();
        assert_eq!(served.path, path);
        assert_eq!(served.root, workspace.0);
        assert_eq!(
            asset_download(
                State(state.clone()),
                RoutePath(id.clone()),
                HeaderMap::new()
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::UNAUTHORIZED
        );
        let response = asset_download(State(state), RoutePath(id), crate::bearer_headers("token"))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), std::fs::read(path).unwrap());
    }

    #[tokio::test]
    async fn image_generation_saved_paths_resolve_through_authenticated_attachment_pipeline() {
        let workspace = ImageWorkspace::new();
        for name in [
            "with space.png",
            "中文图.png",
            "literal%.png",
            "literal%20.png",
            "plot(1).png",
            "hash#name.png",
            "bracket[1].png",
        ] {
            let path = workspace.image(name);
            assert_image_pipeline(
                &workspace,
                &path,
                json!({
                    "type":"imageGeneration", "id":"img", "status":"completed",
                    "revisedPrompt":"a test image", "result":TEST_IMAGE, "savedPath":path,
                }),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn image_generation_legacy_result_file_uri_retains_parenthesized_filename() {
        let workspace = ImageWorkspace::new();
        let path = workspace.image("plot(1).png");
        assert_image_pipeline(&workspace, &path, json!({
            "type":"imageGeneration", "id":"img", "status":"completed",
            "revisedPrompt":"a test image", "result":format!("Saved to: file://{}", path.display()),
        })).await;
    }

    #[tokio::test]
    async fn image_generation_legacy_encoded_file_uri_is_not_double_encoded() {
        let workspace = ImageWorkspace::new();
        let path = workspace.image("中文 %20 (#1).png");
        let uri = reqwest::Url::from_file_path(&path).unwrap();
        assert_image_pipeline(
            &workspace,
            &path,
            json!({
                "type":"imageGeneration", "id":"img", "status":"completed",
                "revisedPrompt":"a test image", "result":format!("Saved to: <{uri}>"),
            }),
        )
        .await;
    }

    #[test]
    fn image_generation_result_only_keeps_a_bounded_typed_image_without_raw_output() {
        use base64::Engine as _;
        let call = mapped(
            json!({"type":"imageGeneration", "id":"img", "status":"completed",
            "revisedPrompt":"a test image", "result":TEST_IMAGE}),
        )
        .remove(0);
        assert_eq!(call.input["prompt"], "a test image");
        assert!(call.output.is_none());
        assert!(call.metadata.unwrap().get("result").is_none());
        let content = call.content.expect("result-only image must not disappear");
        assert_eq!(content[0]["type"], "file");
        assert_eq!(content[0]["mime"], "image/png");
        let uri = content[0]["uri"].as_str().unwrap();
        let encoded = uri.strip_prefix("data:image/png;base64,").unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(
            crate::platform::uploads::sniff_upload_kind(&bytes)
                .unwrap()
                .mime,
            "image/png"
        );
        assert_eq!(
            bytes,
            base64::engine::general_purpose::STANDARD
                .decode(TEST_IMAGE)
                .unwrap()
        );
    }

    #[test]
    fn image_generation_unusable_result_reports_unavailability_not_raw_bytes() {
        for result in [
            "not an image".to_owned(),
            "A".repeat(crate::agents::attachment_images::MAX_IMAGE_BYTES * 2),
        ] {
            let call = mapped(
                json!({"type":"imageGeneration", "id":"img", "status":"completed",
                "result":result}),
            )
            .remove(0);
            assert!(call.output.is_none());
            assert_eq!(call.content.unwrap()[0]["type"], "text");
        }
    }

    #[tokio::test]
    async fn image_generation_invalid_inline_has_no_authenticated_original_download() {
        use axum::extract::{Path as RoutePath, State};
        use axum::http::{HeaderMap, StatusCode};
        use base64::Engine as _;
        use sha2::Digest as _;
        for result in [
            "not an image".to_owned(),
            "A".repeat(crate::agents::attachment_images::MAX_IMAGE_BYTES * 2),
            base64::engine::general_purpose::STANDARD.encode(b"not PNG bytes"),
            format!("{TEST_IMAGE}!"),
        ] {
            let workspace = ImageWorkspace::new();
            let state = crate::test_state("admin", vec![crate::test_device("d1", "token")]);
            let mut rows = mapper::item(
                "thread",
                "turn",
                &json!({"type":"imageGeneration","id":"bad-inline","status":"completed","result":result}),
                0,
            );
            crate::agents::message_images::attach_to_items(
                &state,
                "thread",
                workspace.0.to_str(),
                &mut rows,
            )
            .await;
            let mut ids = rows
                .iter()
                .flat_map(|row| row.image_assets.iter().flatten())
                .filter(|asset| !asset.asset_id.is_empty())
                .map(|asset| asset.asset_id.clone())
                .collect::<Vec<_>>();
            // Also probe the exact content-addressed id the non-PNG payload
            // would receive if wrongly admitted as a generic original file.
            if let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(result.trim_end_matches('!'))
            {
                let hash = hex::encode(sha2::Sha256::digest(&decoded));
                let root = std::fs::canonicalize(crate::state_dir().unwrap())
                    .unwrap()
                    .join("message-images/originals");
                let extension = if result.starts_with("iVBOR") {
                    "png"
                } else {
                    "bin"
                };
                let path = root.join(format!("{hash}.{extension}"));
                ids.push(format!(
                    "ai_{}",
                    hex::encode(sha2::Sha256::digest(
                        format!("thread\n{}", path.display()).as_bytes()
                    ))
                ));
            }
            for id in ids {
                assert_eq!(
                    crate::agents::message_images::image_content(
                        State(state.clone()),
                        RoutePath((id.clone(), "download".into())),
                        HeaderMap::new()
                    )
                    .await
                    .unwrap_err()
                    .0,
                    StatusCode::UNAUTHORIZED
                );
                let result = crate::agents::message_images::image_content(
                    State(state.clone()),
                    RoutePath((id, "download".into())),
                    crate::bearer_headers("token"),
                )
                .await;
                assert!(
                    matches!(result, Err((StatusCode::NOT_FOUND, _))),
                    "invalid generated result acquired an authenticated downloadable original"
                );
            }
            assert!(rows
                .iter()
                .flat_map(|row| row.image_assets.iter().flatten())
                .all(|asset| asset.unavailable_reason.is_some()
                    && asset.url.is_empty()
                    && asset.download_url.is_none()
                    && asset.version_url.is_none()));
            if result.ends_with('!') {
                let assets = rows
                    .iter()
                    .flat_map(|row| row.image_assets.iter().flatten())
                    .collect::<Vec<_>>();
                assert_eq!(assets.len(), 1);
                assert_eq!(
                    assets[0].unavailable_reason.as_deref(),
                    Some("invalid_inline_encoding")
                );
                // Failure metadata does not duplicate inline bytes; the typed
                // source, not UI text/output, retains the failed original.
                assert!(assets[0].src.is_empty());
                let AgentPart::Tool(call) = &rows[0].part else {
                    panic!("expected generated image tool");
                };
                assert_eq!(
                    call.content.as_ref().unwrap()[0]["uri"],
                    format!("data:image/png;base64,{result}")
                );
            }
            for row in &rows {
                if let AgentPart::Tool(call) = &row.part {
                    assert!(call.output.is_none());
                    assert!(call.metadata.as_ref().unwrap().get("result").is_none());
                    if let Some(content) = &call.content {
                        for entry in content.as_array().unwrap() {
                            if entry["type"] == "text" {
                                assert!(entry["text"].as_str().unwrap().contains("unavailable"));
                                assert!(entry["text"].as_str().unwrap().len() < 256);
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn image_generation_inline_original_survives_terminal_size_limit_and_downloads_exact_bytes(
    ) {
        use axum::extract::{Path as RoutePath, State};
        use axum::http::{HeaderMap, StatusCode};
        use base64::Engine as _;
        for large in [false, true] {
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(TEST_IMAGE)
                .unwrap();
            if large {
                bytes.resize(crate::agents::attachment_images::MAX_IMAGE_BYTES + 1, 0);
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            let state = crate::test_state("admin", vec![crate::test_device("d1", "token")]);
            let mut rows = mapper::item(
                "thread",
                "turn",
                &json!({"type":"imageGeneration","id":"inline","result":encoded,"status":"completed"}),
                0,
            );
            crate::agents::message_images::attach_to_items(&state, "thread", None, &mut rows).await;
            let assets = rows
                .iter()
                .flat_map(|row| row.image_assets.iter().flatten())
                .collect::<Vec<_>>();
            assert_eq!(assets.len(), 1);
            let asset = assets[0];
            assert!(asset.unavailable_reason.is_none());
            assert_eq!(asset.mime, "image/png");
            assert_eq!(asset.size, Some(bytes.len() as u64));
            let AgentPart::Tool(call) = &rows[0].part else {
                panic!("expected generated image tool");
            };
            assert!(call.output.is_none());
            assert!(call.metadata.as_ref().unwrap().get("result").is_none());
            assert_eq!(call.content.as_ref().unwrap()[0]["uri"], asset.url);
            assert_eq!(
                crate::agents::message_images::image_content(
                    State(state.clone()),
                    RoutePath((asset.asset_id.clone(), "download".into())),
                    HeaderMap::new()
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::UNAUTHORIZED
            );
            let response = crate::agents::message_images::image_content(
                State(state),
                RoutePath((asset.asset_id.clone(), "download".into())),
                crate::bearer_headers("token"),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let downloaded = axum::body::to_bytes(response.into_body(), bytes.len() + 1)
                .await
                .unwrap();
            assert_eq!(downloaded.as_ref(), bytes);
        }
    }

    #[tokio::test]
    async fn image_generation_paths_do_not_bypass_canonical_session_fence() {
        use axum::extract::{Path as RoutePath, State};
        use axum::http::{HeaderMap, StatusCode};
        use sha2::Digest as _;
        let workspace = ImageWorkspace::new();
        let outside = ImageWorkspace::new();
        let path = outside.image("outside.png");
        let state = crate::test_state("admin", vec![crate::test_device("d1", "token")]);
        let traversal = format!(
            "Saved to: file://{}/generated_images/%2e%2e/%2e%2e/{}/generated_images/outside.png",
            workspace.0.display(),
            outside.0.file_name().unwrap().to_str().unwrap()
        );
        let mut values = vec![
            json!({"savedPath":path}),
            json!({"savedPath":"relative.png"}),
            json!({"savedPath":"https://example.invalid/image.png"}),
            json!({"result":"Saved to: file://example.invalid/image.png"}),
            json!({"result":traversal}),
        ];
        #[cfg(unix)]
        {
            let link = workspace.0.join("generated_images/escape.png");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            values.push(json!({"savedPath":link}));
        }
        for value in values {
            let mut value = value;
            value["type"] = json!("imageGeneration");
            value["id"] = json!("img");
            let mut rows = mapper::item("thread", "turn", &value, 0);
            let raw_parts = rows.iter().map(|row| row.part.clone()).collect::<Vec<_>>();
            crate::agents::message_images::attach_to_items(
                &state,
                "thread",
                workspace.0.to_str(),
                &mut rows,
            )
            .await;
            assert!(rows.iter().all(|row| row.image_assets.is_none()));
            assert_eq!(
                rows.iter().map(|row| row.part.clone()).collect::<Vec<_>>(),
                raw_parts
            );
            assert!(!serde_json::to_string(&rows).unwrap().contains(TEST_IMAGE));
            assert!(state
                .message_images
                .served(&crate::asset_id(&path))
                .is_none());
            let legacy = crate::asset_id(&path);
            assert_eq!(
                crate::platform::assets::asset_download(
                    State(state.clone()),
                    RoutePath(legacy.clone()),
                    HeaderMap::new()
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::UNAUTHORIZED
            );
            assert_eq!(
                crate::platform::assets::asset_download(
                    State(state.clone()),
                    RoutePath(legacy),
                    crate::bearer_headers("token")
                )
                .await
                .unwrap_err()
                .0,
                StatusCode::NOT_FOUND
            );
            let scoped = format!(
                "ai_{}",
                hex::encode(sha2::Sha256::digest(
                    format!("thread\n{}", path.display()).as_bytes()
                ))
            );
            assert!(state.message_images.served(&scoped).is_none());
            for variant in ["original", "download", "version"] {
                assert_eq!(
                    crate::agents::message_images::image_content(
                        State(state.clone()),
                        RoutePath((scoped.clone(), variant.into())),
                        HeaderMap::new()
                    )
                    .await
                    .unwrap_err()
                    .0,
                    StatusCode::UNAUTHORIZED
                );
                assert_eq!(
                    crate::agents::message_images::image_content(
                        State(state.clone()),
                        RoutePath((scoped.clone(), variant.into())),
                        crate::bearer_headers("token")
                    )
                    .await
                    .unwrap_err()
                    .0,
                    StatusCode::NOT_FOUND
                );
            }
        }
    }
    #[test]
    fn patch_counts_do_not_confuse_hunk_content_with_file_headers() {
        assert_eq!(
            patch_counts("--- a/f\n+++ b/f\n@@ -1 +1 @@\n--- content\n+++ content"),
            Some((1, 1))
        );
        assert_eq!(patch_counts("unknown patch format"), None);
        let generated = mapper::item(
            "thread",
            "turn",
            &json!({
                "type": "imageGeneration",
                "id": "img",
                "status": "completed",
                "revisedPrompt": "a kitten",
                "result": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAAB",
                "savedPath": "/home/ryu/.codex/generated_images/kitten.png"
            }),
            0,
        );
        assert!(generated.iter().all(|row| match &row.part {
            AgentPart::Tool(call) =>
                call.output.is_none()
                    && call
                        .metadata
                        .as_ref()
                        .is_none_or(|meta| meta.get("result").is_none()),
            AgentPart::Text { text } =>
                text == "![](<file:///home/ryu/.codex/generated_images/kitten.png>)",
            _ => true,
        }));
        assert!(generated
            .iter()
            .any(|row| matches!(row.part, AgentPart::Text { .. })));
        let call = mapped(json!({"type":"sleep","id":"sleep","durationMs":50})).remove(0);
        assert_eq!(call.input["durationMs"], 50);
        assert!(call.metadata.unwrap().get("durationMs").is_none());
    }
    #[test]
    fn shell_preserves_exit_duration_script_and_process_without_inventing_background() {
        let rows = mapped(
            json!({"type":"commandExecution","id":"shell","status":"completed","source":"unifiedExecStartup","command":"echo test","cwd":"/fixture","processId":"42","exitCode":7,"durationMs":125,"scriptPath":"scripts/check.py","pluginId":"fixture","aggregatedOutput":"test"}),
        );
        let call = &rows[0];
        assert_eq!(call.name, "shell");
        assert_eq!(call.output, Some(json!("test")));
        assert_eq!(call.state, ToolCallStatus::Completed);
        assert!(!call.background);
        assert!(call.time.is_empty());
        let meta = call.metadata.as_ref().unwrap();
        assert_eq!(meta["exit"], 7);
        assert_eq!(meta["durationMs"], 125);
        assert_eq!(meta["scriptPath"], "scripts/check.py");
        assert_eq!(meta["processId"], "42");
        assert!(meta.get("aggregatedOutput").is_none());
    }
    #[test]
    fn edit_preserves_patch_rename_and_counts_without_reading_the_worktree() {
        let rows = mapped(
            json!({"type":"fileChange","id":"edit","status":"completed","changes":[{"path":"old.rs","kind":{"type":"update","move_path":"new.rs"},"diff":"--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-old\n+new\n"}]}),
        );
        let file = &rows[0].metadata.as_ref().unwrap()["files"][0];
        assert_eq!(file["path"], "old.rs");
        assert_eq!(file["move_path"], "new.rs");
        assert_eq!(file["additions"], 1);
        assert_eq!(file["deletions"], 1);
        assert!(file["diff"].as_str().unwrap().contains("+new"));
    }
    #[test]
    fn completed_collaboration_call_keeps_child_lifecycle_separate_and_rows_stable() {
        let mut wire = json!({"type":"collabAgentToolCall","id":"spawn","tool":"wait","status":"completed","senderThreadId":"thread","receiverThreadIds":["child-b","child-a"],"prompt":null,"agentsStates":{"child-a":{"status":"running","message":null},"child-b":{"status":"errored","message":"failed"}}});
        let first = mapper::item("thread", "turn", &wire, 0);
        wire["receiverThreadIds"] = json!(["child-a", "child-b"]);
        let second = mapper::item("thread", "turn", &wire, 0);
        assert_eq!(first, second);
        let calls = mapped(wire);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].child_session_id.as_deref(), Some("child-a"));
        assert_eq!(calls[0].state, ToolCallStatus::Completed);
        assert_eq!(calls[0].metadata.as_ref().unwrap()["status"], "working");
        assert_eq!(calls[1].metadata.as_ref().unwrap()["status"], "failed");
        assert_eq!(calls[1].output, Some(json!("failed")));
    }
    #[test]
    fn missing_child_state_is_not_tool_completion_and_activity_identity_is_exact() {
        let call = mapped(json!({"type":"collabAgentToolCall","id":"spawn","status":"completed","receiverThreadIds":["child"],"agentsStates":{}})).remove(0);
        assert!(call.metadata.unwrap().get("status").is_none());
        let call = mapped(json!({"type":"subAgentActivity","id":"activity","kind":"interacted","agentThreadId":"child","agentPath":"/root/review"})).remove(0);
        assert_eq!(call.child_session_id.as_deref(), Some("child"));
        assert_eq!(call.title.as_deref(), Some("/root/review"));
        assert!(call.metadata.unwrap().get("status").is_none());
    }
    #[test]
    fn dynamic_web_sleep_and_mcp_outputs_are_not_silently_dropped() {
        for wire in [
            json!({"type":"dynamicToolCall","id":"dynamic","tool":"fixture","status":"failed","arguments":{"a":1},"contentItems":[{"type":"inputText","text":"result"}]}),
            json!({"type":"webSearch","id":"web","query":"fixture","results":[{"url":"https://example.test"}]}),
            json!({"type":"sleep","id":"sleep","durationMs":50}),
            json!({"type":"mcpToolCall","id":"mcp","server":"fixture","tool":"read","status":"failed","error":{"message":"unavailable"}}),
        ] {
            assert_eq!(mapped(wire).len(), 1);
        }
        let call =
            mapped(json!({"type":"sleep","id":"sleep","durationMs":50,"status":"inProgress"}))
                .remove(0);
        assert_eq!(call.state, ToolCallStatus::Running);
    }
}
