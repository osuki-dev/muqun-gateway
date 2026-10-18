use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::endpoint::DeepseekEndpoint;
use crate::agents::domain::{ModelRef, SessionQuery};
use crate::agents::ports::agent::{append_attachment_paths, AgentError};

/// Largest upstream response body we will buffer.
pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// How much of an upstream error body is kept for logs.
const ERROR_SNIPPET_BYTES: usize = 512;
/// Delivery modes `session/prompt` accepts.
const DELIVERY_MODES: [&str; 2] = ["queue", "steer"];
const HISTORY_PAGE_MESSAGES: usize = 50;

#[derive(Deserialize)]
struct ModelSelection {
    provider: String,
    model: String,
    #[serde(rename = "reasoningEffort")]
    reasoning_effort: Option<String>,
}

pub(super) enum ReadError {
    TooLarge,
    Network,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("body exceeds size cap"),
            Self::Network => f.write_str("body read failed"),
        }
    }
}

/// Read a response body, failing rather than buffering more than `cap` bytes.
pub(super) async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, ReadError> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(ReadError::TooLarge);
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| ReadError::Network)? {
        if out.len() + chunk.len() > cap {
            return Err(ReadError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Truncate to at most `max` bytes on a char boundary, for log lines.
fn truncate_for_log(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The session id named in RPC args (`request.sessionId`), or a placeholder.
fn session_id_of(args: &Value) -> String {
    args.pointer("/request/sessionId")
        .and_then(Value::as_str)
        .map(|s| truncate_for_log(s, 128).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Keep an upstream error code short and free of anything but token characters.
fn sanitize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
        .take(64)
        .collect()
}

#[derive(Clone)]
pub struct DeepseekClient {
    pub endpoint: DeepseekEndpoint,
    http: Client,
}

impl DeepseekClient {
    pub fn new(endpoint: DeepseekEndpoint) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { endpoint, http }
    }

    /// Invoke a Typert Remote method via HTTP POST `/api/{namespace}/{method}`.
    ///
    /// DeepSeek Harness expects:
    /// Payload:
    /// ```json
    /// {
    ///   "type": "client-request",
    ///   "rpcId": "...",
    ///   "method": "{namespace}/{method}",
    ///   "payload": {
    ///     "args": { ... }
    ///   }
    /// }
    /// ```
    /// Response:
    /// ```json
    /// {
    ///   "type": "server-response",
    ///   "rpcId": "...",
    ///   "result": {
    ///     "ok": true,
    ///     "value": ...
    ///   }
    /// }
    /// ```
    pub async fn call_remote(
        &self,
        namespace: &str,
        method: &str,
        args: Value,
    ) -> Result<Value, AgentError> {
        let url = format!("{}/api/{namespace}/{method}", self.endpoint.url);
        let rpc_id = format!("req_{}", Uuid::new_v4().simple());
        let full_method = format!("{namespace}/{method}");
        let args_for_id = args.clone();

        let wire_payload = json!({
            "type": "client-request",
            "rpcId": rpc_id,
            "method": full_method,
            "payload": {
                "args": args
            }
        });

        let mut req = self.http.post(&url);
        req = req.header("Host", self.endpoint.authority());
        if let Some(cookie) = self.endpoint.auth_cookie() {
            req = req.header("Cookie", cookie);
        }
        if let Some(ref token) = self.endpoint.token {
            req = req.bearer_auth(token);
        }

        let resp = req.json(&wire_payload).send().await.map_err(|e| {
            tracing::warn!(target: "deepseek", "{full_method}: request failed: {e}");
            AgentError::Network(if e.is_timeout() {
                "upstream request timed out".to_string()
            } else {
                "connection to upstream failed".to_string()
            })
        })?;

        let status = resp.status();
        if !status.is_success() {
            let snippet = match read_capped(resp, ERROR_SNIPPET_BYTES).await {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(_) => String::new(),
            };
            tracing::warn!(
                target: "deepseek",
                "{full_method}: upstream returned HTTP {status}: {}",
                truncate_for_log(&snippet, ERROR_SNIPPET_BYTES)
            );
            if status == reqwest::StatusCode::NOT_FOUND && namespace == "session" {
                return Err(AgentError::SessionNotFound(session_id_of(&args_for_id)));
            }
            return Err(AgentError::RequestFailed(format!(
                "upstream returned HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = read_capped(resp, MAX_RESPONSE_BYTES).await.map_err(|e| {
            tracing::warn!(target: "deepseek", "{full_method}: reading response failed: {e}");
            match e {
                ReadError::TooLarge => {
                    AgentError::Protocol("upstream response too large".to_string())
                }
                ReadError::Network => {
                    AgentError::Network("reading upstream response failed".to_string())
                }
            }
        })?;

        if bytes.is_empty() {
            return Ok(json!({ "success": true }));
        }

        let envelope: Value = serde_json::from_slice(&bytes).map_err(|e| {
            tracing::warn!(target: "deepseek", "{full_method}: invalid JSON response: {e}");
            AgentError::Protocol("invalid JSON RPC response".to_string())
        })?;

        // Typert Connection RPC Envelope check:
        // Result is in `envelope.result` or at top-level `envelope`.
        let result_obj = envelope.get("result").unwrap_or(&envelope);
        if let Some(ok) = result_obj.get("ok").and_then(Value::as_bool) {
            if ok {
                Ok(result_obj.get("value").cloned().unwrap_or(Value::Null))
            } else {
                let error = result_obj.get("error");
                let code = error
                    .and_then(|e| e.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown_error");
                let msg = error
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("RPC call failed");

                tracing::warn!(target: "deepseek", "{full_method}: RPC error {code}: {}", truncate_for_log(msg, ERROR_SNIPPET_BYTES));
                // Only a missing session is `SessionNotFound`; a missing preset,
                // workspace or job is a rejected request, not a lost session.
                if code == "session/not-found" {
                    Err(AgentError::SessionNotFound(session_id_of(&args_for_id)))
                } else {
                    Err(AgentError::RequestFailed(format!(
                        "upstream rejected: {}",
                        sanitize_code(code)
                    )))
                }
            }
        } else {
            Ok(envelope)
        }
    }

    /// List sessions from session-controller: `session/list`
    pub async fn list_sessions(&self, _query: &SessionQuery) -> Result<Vec<Value>, AgentError> {
        let args = json!({
            "_request": {}
        });
        let result = self.call_remote("session", "list", args).await?;
        if let Some(items) = result.get("items").and_then(Value::as_array) {
            Ok(items.clone())
        } else if let Some(items) = result.get("sessions").and_then(Value::as_array) {
            Ok(items.clone())
        } else if let Some(items) = result.as_array() {
            Ok(items.clone())
        } else {
            Ok(vec![])
        }
    }

    /// Create or resume a session: `session/create`
    pub async fn create_session(
        &self,
        directory: Option<&str>,
        _model: Option<&ModelRef>,
        agent_preset: Option<&str>,
    ) -> Result<String, AgentError> {
        let session_id = format!("session-{}", Uuid::new_v4());
        let cwd = directory.unwrap_or(".");
        let mut req_body = json!({
            "sessionId": session_id,
            "cwd": cwd,
        });
        // No preset unless the caller named one; DSH resolves its own default.
        if let Some(preset) = agent_preset.filter(|p| !p.is_empty()) {
            req_body["agentPreset"] = json!(preset);
        }

        let args = json!({ "request": req_body });
        let result = self.call_remote("session", "create", args).await?;
        Ok(result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or(session_id))
    }

    /// Submit prompt to session: `session/prompt`
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<Value, AgentError> {
        // DeepSeek has no file parts; it reads files with its own tools, so
        // any path that reached this far travels in the text.
        let text = &append_attachment_paths(text, attachments);
        let mode = delivery.unwrap_or("steer");
        if !DELIVERY_MODES.contains(&mode) {
            return Err(AgentError::RequestFailed(
                "unsupported delivery mode".to_string(),
            ));
        }
        let req_id = format!("req_{}", Uuid::new_v4().simple());
        let content_parts = vec![json!({
            "type": "text",
            "text": text,
        })];

        let args = json!({
            "request": {
                "sessionId": session_id,
                "requestId": req_id,
                "mode": mode,
                "content": content_parts,
            }
        });

        self.call_remote("session", "prompt", args).await
    }

    /// Cancel active turn: `session/cancel`
    pub async fn cancel(&self, session_id: &str) -> Result<Value, AgentError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "cancel", args).await
    }

    /// Rename session: `session/rename`
    pub async fn rename(&self, session_id: &str, title: &str) -> Result<Value, AgentError> {
        let args = json!({
            "request": {
                "sessionId": session_id,
                "title": title,
            }
        });
        self.call_remote("session", "rename", args).await
    }

    /// Fork session: `session/fork`
    pub async fn fork(&self, session_id: &str) -> Result<Value, AgentError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "fork", args).await
    }

    /// Read historical messages page: `session/page`
    pub async fn get_page(&self, session_id: &str, limit: usize) -> Result<Value, AgentError> {
        if limit == 0 {
            return Ok(json!({ "records": [], "hasMore": false }));
        }
        // rc.2 projections are a cold-safe observation at an exact log cut.
        // Do not use list hints (which may be cached) or follow (which promotes
        // cold sessions). Every backwards page must retain this same cut.
        let projections = self.get_projections(session_id).await?;
        if projections.is_null() {
            return Err(AgentError::SessionNotFound(session_id.to_string()));
        }
        let through_seq = projections
            .get("asOfSeq")
            .and_then(Value::as_i64)
            .filter(|seq| (-1..=9_007_199_254_740_991).contains(seq))
            .ok_or_else(|| {
                AgentError::Protocol("session projections lack a valid cursor".into())
            })?;
        let mut before_seq = through_seq + 1;
        let mut remaining = limit;
        let mut records = Vec::new();
        let mut retained_bytes = 0;
        let has_more = loop {
            let page = self
                .call_remote(
                    "session",
                    "page",
                    json!({ "request": {
                "address": { "kind": "session", "sessionId": session_id },
                "throughSeq": through_seq,
                "beforeSeq": before_seq,
                "maxMessages": remaining.min(HISTORY_PAGE_MESSAGES),
            } }),
                )
                .await?;
            let page_records = page
                .get("records")
                .and_then(Value::as_array)
                .ok_or_else(|| AgentError::Protocol("history page lacks records".into()))?;
            let has_more = page
                .get("hasMore")
                .and_then(Value::as_bool)
                .ok_or_else(|| AgentError::Protocol("history page lacks hasMore".into()))?;
            let mut previous_seq = None;
            let mut messages = 0;
            for record in page_records {
                let event = record
                    .get("event")
                    .filter(|_| record.get("type").and_then(Value::as_str) == Some("event"))
                    .ok_or_else(|| AgentError::Protocol("invalid history record".into()))?;
                let seq = event
                    .get("seq")
                    .and_then(Value::as_i64)
                    .filter(|seq| *seq >= 0 && *seq <= through_seq && *seq < before_seq)
                    .ok_or_else(|| {
                        AgentError::Protocol("history record is outside the frozen cut".into())
                    })?;
                if previous_seq.is_some_and(|previous| seq != previous + 1) {
                    return Err(AgentError::Protocol(
                        "history page is not contiguous".into(),
                    ));
                }
                previous_seq = Some(seq);
                // The published page budget counts only append-origin human
                // and assistant messages, not tool results or bookkeeping.
                if matches!(
                    event.get("type").and_then(Value::as_str),
                    Some("user/message" | "assistant/message")
                ) && event.get("surfaceOp").and_then(Value::as_str) == Some("append")
                {
                    messages += 1;
                }
            }
            if before_seq > 0 && previous_seq != Some(before_seq - 1) {
                return Err(AgentError::Protocol("history page skipped records".into()));
            }
            retained_bytes += serde_json::to_vec(page_records)
                .map_err(|_| AgentError::Protocol("invalid history page".into()))?
                .len();
            if retained_bytes > MAX_RESPONSE_BYTES {
                return Err(AgentError::Protocol("upstream history too large".into()));
            }
            let first_seq = page_records
                .first()
                .and_then(|record| record.pointer("/event/seq"))
                .and_then(Value::as_i64);
            if has_more && first_seq.is_none_or(|seq| seq == 0) {
                return Err(AgentError::Protocol("history page cannot advance".into()));
            }
            if !has_more && first_seq.is_some_and(|seq| seq > 0) {
                return Err(AgentError::Protocol(
                    "history page omitted earlier records".into(),
                ));
            }
            records.splice(0..0, page_records.iter().cloned());
            remaining = remaining.saturating_sub(messages);
            if !has_more || remaining == 0 {
                break has_more;
            }
            before_seq = first_seq.expect("nonempty page with more history");
        };
        Ok(json!({ "records": records, "hasMore": has_more }))
    }

    /// Read session projections: `session/projections`
    pub async fn get_projections(&self, session_id: &str) -> Result<Value, AgentError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "projections", args).await
    }

    /// List the Agent presets `session/create` accepts: `agentPresets/list`.
    pub async fn agent_presets(&self) -> Result<Value, AgentError> {
        self.call_remote("agentPresets", "list", json!({})).await
    }

    /// Fetch available model catalog: `session/modelCatalog`
    pub async fn model_catalog(&self) -> Result<Value, AgentError> {
        self.call_remote("session", "modelCatalog", json!({})).await
    }

    /// Switch active model for session: `session/selectModel`
    pub async fn select_model(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<ModelRef, AgentError> {
        let mut selection = json!({
            "sessionId": session_id,
            "provider": provider,
            "model": model,
        });
        if let Some(effort) = reasoning_effort {
            selection["reasoningEffort"] = json!(effort);
        }

        let args = json!({
            "request": selection
        });

        let result = self.call_remote("session", "selectModel", args).await?;
        let selected: ModelSelection =
            serde_json::from_value(result.get("selected").cloned().unwrap_or(Value::Null))
                .map_err(|_| {
                    AgentError::Protocol("model selection response lacks selected model".into())
                })?;
        if selected.provider.is_empty() || selected.model.is_empty() {
            return Err(AgentError::Protocol(
                "model selection response has an empty model".into(),
            ));
        }
        Ok(ModelRef {
            provider_id: selected.provider,
            model_id: selected.model,
            variant: selected.reasoning_effort,
        })
    }

    /// List files in workspace: `workspaceFiles/list`
    pub async fn workspace_files_list(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<Value, AgentError> {
        if session_id.is_empty() || path.is_empty() {
            return Err(AgentError::RequestFailed(
                "workspace files require a session scope and nonempty path".into(),
            ));
        }
        let args = json!({
            "workspaceFileScopeId": session_id,
            "path": path,
        });
        self.call_remote("workspaceFiles", "list", args).await
    }

    /// Read file metadata: `workspaceFiles/stat`
    pub async fn workspace_files_stat(
        &self,
        session_id: &str,
        path: &str,
    ) -> Result<Value, AgentError> {
        if session_id.is_empty() || path.is_empty() {
            return Err(AgentError::RequestFailed(
                "workspace files require a session scope and nonempty path".into(),
            ));
        }
        let args = json!({
            "workspaceFileScopeId": session_id,
            "path": path,
        });
        self.call_remote("workspaceFiles", "stat", args).await
    }

    /// The Host resolves `agentId` from an existing live Session, never a cwd.
    pub async fn file_references(
        &self,
        session_id: &str,
        query: &str,
    ) -> Result<Value, AgentError> {
        self.call_remote(
            "fileReferences",
            "list",
            json!({ "agentId": session_id, "query": query }),
        )
        .await
    }
}

#[cfg(test)]
pub(super) mod test_server {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve `status` and `body` to every connection; returns the base URL.
    pub(crate) async fn serve(status: u16, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Read headers, then the declared body, so the client is
                    // never reset mid-write.
                    loop {
                        let Ok(n) = sock.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:"))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if buf.len() >= pos + 4 + len {
                                break;
                            }
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://{addr}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rpc_envelope_success() {
        let raw = json!({
            "type": "server-response",
            "rpcId": "req_1",
            "result": {
                "ok": true,
                "value": { "created": true, "sessionId": "ses_123" }
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        let result_obj = envelope.get("result").unwrap();
        assert_eq!(result_obj.get("ok").and_then(Value::as_bool), Some(true));
        assert_eq!(
            result_obj
                .get("value")
                .and_then(|v| v.get("sessionId"))
                .and_then(Value::as_str),
            Some("ses_123")
        );
    }

    #[test]
    fn parses_rpc_envelope_error() {
        let raw = json!({
            "type": "server-response",
            "rpcId": "req_2",
            "result": {
                "ok": false,
                "error": {
                    "code": "session/not-found",
                    "message": "Session ses_999 does not exist",
                    "details": {}
                }
            }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        let result_obj = envelope.get("result").unwrap();
        assert_eq!(result_obj.get("ok").and_then(Value::as_bool), Some(false));
    }

    fn client_for(url: String) -> DeepseekClient {
        DeepseekClient::new(DeepseekEndpoint::new(url, None, None))
    }

    async fn rpc_server(app: axum::Router) -> (DeepseekClient, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = client_for(format!("http://{}", listener.local_addr().unwrap()));
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (client, task)
    }

    #[tokio::test]
    async fn history_pages_use_one_authoritative_cut_and_count_only_append_messages() {
        use axum::{routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        // rc.2 history.ts: dense events, an inclusive frozen throughSeq,
        // exclusive beforeSeq, and only append user/assistant budget counts.
        let source: Vec<Value> = (0..=360)
            .map(|seq| {
                let event = match seq % 3 {
                    0 => json!({ "type": "user/message", "seq": seq, "time": seq, "surfaceOp": "append",
                        "data": { "id": format!("u-{seq}"), "role": "user", "content": [{ "type": "text", "text": "hi" }], "source": { "kind": "user" } } }),
                    1 => json!({ "type": "tool/result", "seq": seq, "time": seq, "surfaceOp": "append",
                        "data": { "turn": 1, "step": 1, "message": { "id": format!("t-{seq}"), "role": "tool", "toolCallId": format!("c-{seq}"), "content": [{ "type": "text", "text": "result" }], "source": { "kind": "tool" } } } }),
                    _ => json!({ "type": "step/end", "seq": seq, "time": seq, "data": { "turn": 1, "step": 1 } }),
                };
                json!({ "type": "event", "event": event })
            })
            .collect();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let observations = Arc::new(AtomicUsize::new(0));
        let reads = observations.clone();
        let app = Router::new()
            .route("/api/session/projections", post(move || { let reads = reads.clone(); async move {
                let cursor = 359 + reads.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "result": { "ok": true, "value": { "asOfSeq": cursor, "values": {} } } }))
            } }))
            .route("/api/session/page", post(move |Json(body): Json<Value>| {
                let source = source.clone();
                let seen = seen.clone();
                async move {
                    let request = body.pointer("/payload/args/request").unwrap();
                    assert_eq!(request["address"], json!({ "kind": "session", "sessionId": "s" }));
                    let through = request["throughSeq"].as_u64().unwrap();
                    // The actual log may have advanced; no post-cut events
                    // should be requested or returned to the mapper.
                    assert!(through <= 360, "released Host rejects future cursors");
                    assert_eq!(through, 359);
                    let before = request["beforeSeq"].as_u64().unwrap() as usize;
                    let max = request["maxMessages"].as_u64().unwrap() as usize;
                    assert!(max > 0 && max <= 50);
                    seen.lock().unwrap().push((through, before, max));
                    let mut count = 0;
                    let mut cut = 0;
                    for index in (0..before).rev() {
                        if source[index].pointer("/event/type").and_then(Value::as_str) == Some("user/message") {
                            count += 1;
                            if count == max { cut = index; break; }
                        }
                    }
                    Json(json!({ "result": { "ok": true, "value": {
                        "records": source[cut..before], "hasMore": cut > 0
                    } } }))
                }
            }));
        let (client, server) = rpc_server(app).await;
        let history = client.get_page("s", 110).await.unwrap();
        let records = history["records"].as_array().unwrap();
        assert_eq!(records.len(), 330);
        assert_eq!(records.first().unwrap()["event"]["seq"], 30);
        assert_eq!(records.last().unwrap()["event"]["seq"], 359);
        assert_eq!(history["hasMore"], true);
        assert_eq!(observations.load(Ordering::SeqCst), 1);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![(359, 360, 50), (359, 210, 50), (359, 60, 10)]
        );
        server.abort();
    }

    #[tokio::test]
    async fn history_rejects_invalid_cursors_and_nonadvancing_or_out_of_cut_pages() {
        use axum::{routing::post, Json, Router};
        for (projections, page) in [
            (Value::Null, json!({})),
            (json!({ "values": {} }), json!({})),
            (json!({ "asOfSeq": -2 }), json!({})),
            (
                json!({ "asOfSeq": 2 }),
                json!({ "records": [], "hasMore": true }),
            ),
            (
                json!({ "asOfSeq": 2 }),
                json!({ "records": [{ "type": "event", "event": { "seq": 3 } }], "hasMore": false }),
            ),
        ] {
            let app = Router::new()
                .route(
                    "/api/session/projections",
                    post(move || {
                        let value = projections.clone();
                        async move { Json(json!({ "result": { "ok": true, "value": value } })) }
                    }),
                )
                .route(
                    "/api/session/page",
                    post(move || {
                        let value = page.clone();
                        async move { Json(json!({ "result": { "ok": true, "value": value } })) }
                    }),
                );
            let (client, server) = rpc_server(app).await;
            assert!(matches!(
                client.get_page("s", 2).await,
                Err(AgentError::Protocol(_) | AgentError::SessionNotFound(_))
            ));
            server.abort();
        }
        let client = client_for("http://127.0.0.1:1".into());
        assert_eq!(client.get_page("s", 0).await.unwrap()["records"], json!([]));
    }

    #[tokio::test]
    async fn empty_history_uses_the_published_minus_one_cut() {
        use axum::{routing::post, Json, Router};
        let app = Router::new()
            .route("/api/session/projections", post(|| async {
                Json(json!({ "result": { "ok": true, "value": { "asOfSeq": -1, "values": {} } } }))
            }))
            .route("/api/session/page", post(|Json(body): Json<Value>| async move {
                assert_eq!(body.pointer("/payload/args/request/throughSeq"), Some(&json!(-1)));
                assert_eq!(body.pointer("/payload/args/request/beforeSeq"), Some(&json!(0)));
                Json(json!({ "result": { "ok": true, "value": { "records": [], "hasMore": false } } }))
            }));
        let (client, server) = rpc_server(app).await;
        assert_eq!(client.get_page("s", 5).await.unwrap()["records"], json!([]));
        server.abort();
    }

    #[tokio::test]
    async fn workspace_file_methods_send_session_scope_and_preserve_entries() {
        use axum::{routing::post, Json, Router};
        let app = Router::new()
            .route("/api/workspaceFiles/list", post(|Json(body): Json<Value>| async move {
                assert_eq!(body.pointer("/payload/args"), Some(&json!({ "workspaceFileScopeId": "s", "path": "." })));
                Json(json!({ "result": { "ok": true, "value": { "path": "", "entries": [{ "name": "src", "type": "directory" }], "truncated": false } } }))
            }))
            .route("/api/workspaceFiles/stat", post(|Json(body): Json<Value>| async move {
                assert_eq!(body.pointer("/payload/args"), Some(&json!({ "workspaceFileScopeId": "s", "path": "src/lib.rs" })));
                Json(json!({ "result": { "ok": true, "value": { "absolutePath": "/qa/src/lib.rs", "version": "v1", "bytes": 8 } } }))
            }));
        let (client, server) = rpc_server(app).await;
        assert_eq!(
            client.workspace_files_list("s", ".").await.unwrap()["entries"][0]["name"],
            "src"
        );
        assert_eq!(
            client
                .workspace_files_stat("s", "src/lib.rs")
                .await
                .unwrap()["absolutePath"],
            "/qa/src/lib.rs"
        );
        assert!(matches!(
            client.workspace_files_list("s", "").await,
            Err(AgentError::RequestFailed(_))
        ));
        assert!(matches!(
            client.workspace_files_stat("", "src/lib.rs").await,
            Err(AgentError::RequestFailed(_))
        ));
        server.abort();
    }

    #[tokio::test]
    async fn http_error_does_not_leak_body_or_url() {
        let url = client_for(test_server::serve(500, b"secret-internal-detail".to_vec()).await);
        let err = url
            .call_remote("session", "list", json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, AgentError::RequestFailed(_)));
        assert!(msg.contains("HTTP 500"));
        assert!(!msg.contains("secret-internal-detail"));
        assert!(!msg.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn http_404_on_session_method_is_session_not_found() {
        let c = client_for(test_server::serve(404, b"nope".to_vec()).await);
        let err = c.cancel("ses_x").await.unwrap_err();
        assert!(matches!(err, AgentError::SessionNotFound(ref id) if id == "ses_x"));
    }

    fn rpc_error_body(code: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"type":"server-response","result":{"ok":false,
            "error":{"code":code,"message":"Unknown agent preset: general"}}}))
        .unwrap()
    }

    #[tokio::test]
    async fn rejected_preset_is_request_failed_not_session_not_found() {
        let c = client_for(test_server::serve(200, rpc_error_body("agent-preset/not-found")).await);
        let err = c
            .create_session(Some("/tmp"), None, Some("general"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AgentError::RequestFailed(m) if m == "upstream rejected: agent-preset/not-found"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn session_not_found_rpc_code_maps_to_session_not_found() {
        let c = client_for(test_server::serve(200, rpc_error_body("session/not-found")).await);
        let err = c.cancel("ses_x").await.unwrap_err();
        assert!(matches!(err, AgentError::SessionNotFound(ref id) if id == "ses_x"));
    }

    #[tokio::test]
    async fn network_error_does_not_leak_url() {
        // Bind then drop so the port refuses connections.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let c = client_for(format!("http://127.0.0.1:{port}"));
        let err = c.model_catalog().await.unwrap_err();
        assert!(matches!(err, AgentError::Network(_)));
        assert!(!err.to_string().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn oversized_response_is_rejected() {
        let c = client_for(test_server::serve(200, vec![b' '; MAX_RESPONSE_BYTES + 1]).await);
        let err = c.model_catalog().await.unwrap_err();
        assert!(matches!(err, AgentError::Protocol(_)));
    }

    #[tokio::test]
    async fn rpc_error_message_is_not_relayed() {
        let body = json!({"type":"server-response","result":{"ok":false,
            "error":{"code":"boom","message":"/home/user/secret/path"}}});
        let c = client_for(test_server::serve(200, serde_json::to_vec(&body).unwrap()).await);
        let err = c.model_catalog().await.unwrap_err();
        assert!(!err.to_string().contains("/home/user"));
    }

    #[tokio::test]
    async fn create_session_returns_generated_id_when_response_lacks_one() {
        let body = json!({"type":"server-response","result":{"ok":true,"value":{}}});
        let c = client_for(test_server::serve(200, serde_json::to_vec(&body).unwrap()).await);
        let id = c.create_session(Some("/tmp"), None, None).await.unwrap();
        assert!(id.starts_with("session-"));
    }

    #[tokio::test]
    async fn prompt_rejects_unknown_delivery_but_not_attachments() {
        let c = client_for("http://127.0.0.1:1".to_string());
        // Attachments travel in the text, so this gets as far as the network.
        let e = c
            .send_prompt("s", "hi", &["/up/a.png".to_string()], None)
            .await
            .unwrap_err();
        assert!(matches!(e, AgentError::Network(_)));
        let e = c
            .send_prompt("s", "hi", &[], Some("bogus"))
            .await
            .unwrap_err();
        assert!(matches!(e, AgentError::RequestFailed(_)));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_for_log("aé", 2), "a");
        assert_eq!(truncate_for_log("abc", 10), "abc");
    }
}
