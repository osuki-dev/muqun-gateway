use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::endpoint::DeepseekEndpoint;
use crate::agents::domain::{ModelRef, SessionQuery};
use crate::agents::ports::engine::AgentEngineError;

/// Largest upstream response body we will buffer.
pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// How much of an upstream error body is kept for logs.
const ERROR_SNIPPET_BYTES: usize = 512;
/// Delivery modes `session/prompt` accepts.
const DELIVERY_MODES: [&str; 2] = ["queue", "steer"];

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
    ) -> Result<Value, AgentEngineError> {
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
            AgentEngineError::Network(if e.is_timeout() {
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
                return Err(AgentEngineError::SessionNotFound(session_id_of(
                    &args_for_id,
                )));
            }
            return Err(AgentEngineError::RequestFailed(format!(
                "upstream returned HTTP {}",
                status.as_u16()
            )));
        }

        let bytes = read_capped(resp, MAX_RESPONSE_BYTES).await.map_err(|e| {
            tracing::warn!(target: "deepseek", "{full_method}: reading response failed: {e}");
            match e {
                ReadError::TooLarge => {
                    AgentEngineError::Protocol("upstream response too large".to_string())
                }
                ReadError::Network => {
                    AgentEngineError::Network("reading upstream response failed".to_string())
                }
            }
        })?;

        if bytes.is_empty() {
            return Ok(json!({ "success": true }));
        }

        let envelope: Value = serde_json::from_slice(&bytes).map_err(|e| {
            tracing::warn!(target: "deepseek", "{full_method}: invalid JSON response: {e}");
            AgentEngineError::Protocol("invalid JSON RPC response".to_string())
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
                if code.contains("not-found") {
                    Err(AgentEngineError::SessionNotFound(session_id_of(
                        &args_for_id,
                    )))
                } else {
                    Err(AgentEngineError::RequestFailed(format!(
                        "upstream RPC error: {}",
                        sanitize_code(code)
                    )))
                }
            }
        } else {
            Ok(envelope)
        }
    }

    /// List sessions from session-controller: `session/list`
    pub async fn list_sessions(
        &self,
        _query: &SessionQuery,
    ) -> Result<Vec<Value>, AgentEngineError> {
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
    ) -> Result<String, AgentEngineError> {
        let session_id = format!("session-{}", Uuid::new_v4());
        let cwd = directory.unwrap_or(".");
        let mut req_body = json!({
            "sessionId": session_id,
            "cwd": cwd,
        });
        if let Some(preset) = agent_preset {
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
    ) -> Result<Value, AgentEngineError> {
        if !attachments.is_empty() {
            return Err(AgentEngineError::Unsupported("attachments".to_string()));
        }
        let mode = delivery.unwrap_or("steer");
        if !DELIVERY_MODES.contains(&mode) {
            return Err(AgentEngineError::RequestFailed(
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
    pub async fn cancel(&self, session_id: &str) -> Result<Value, AgentEngineError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "cancel", args).await
    }

    /// Rename session: `session/rename`
    pub async fn rename(&self, session_id: &str, title: &str) -> Result<Value, AgentEngineError> {
        let args = json!({
            "request": {
                "sessionId": session_id,
                "title": title,
            }
        });
        self.call_remote("session", "rename", args).await
    }

    /// Fork session: `session/fork`
    pub async fn fork(&self, session_id: &str) -> Result<Value, AgentEngineError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "fork", args).await
    }

    /// Read historical messages page: `session/page`
    pub async fn get_page(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Value, AgentEngineError> {
        let args = json!({
            "request": {
                "address": {
                    "kind": "session",
                    "sessionId": session_id,
                },
                "throughSeq": 999_999_999,
                "maxMessages": limit,
            }
        });
        self.call_remote("session", "page", args).await
    }

    /// Read session projections: `session/projections`
    pub async fn get_projections(&self, session_id: &str) -> Result<Value, AgentEngineError> {
        let args = json!({
            "request": {
                "sessionId": session_id
            }
        });
        self.call_remote("session", "projections", args).await
    }

    /// Fetch available model catalog: `session/modelCatalog`
    pub async fn model_catalog(&self) -> Result<Value, AgentEngineError> {
        self.call_remote("session", "modelCatalog", json!({})).await
    }

    /// Switch active model for session: `session/selectModel`
    pub async fn select_model(
        &self,
        session_id: &str,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Value, AgentEngineError> {
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

        self.call_remote("session", "selectModel", args).await
    }

    /// List files in workspace: `workspaceFiles/list`
    pub async fn workspace_files_list(
        &self,
        cwd: &str,
        path: &str,
    ) -> Result<Value, AgentEngineError> {
        let args = json!({
            "workspaceFileScope": {
                "cwd": cwd,
            },
            "path": path,
        });
        self.call_remote("workspaceFiles", "list", args).await
    }

    /// Read file metadata: `workspaceFiles/stat`
    pub async fn workspace_files_stat(
        &self,
        cwd: &str,
        path: &str,
    ) -> Result<Value, AgentEngineError> {
        let args = json!({
            "workspaceFileScope": {
                "cwd": cwd,
            },
            "path": path,
        });
        self.call_remote("workspaceFiles", "stat", args).await
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

    #[tokio::test]
    async fn http_error_does_not_leak_body_or_url() {
        let url = client_for(test_server::serve(500, b"secret-internal-detail".to_vec()).await);
        let err = url
            .call_remote("session", "list", json!({}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, AgentEngineError::RequestFailed(_)));
        assert!(msg.contains("HTTP 500"));
        assert!(!msg.contains("secret-internal-detail"));
        assert!(!msg.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn http_404_on_session_method_is_session_not_found() {
        let c = client_for(test_server::serve(404, b"nope".to_vec()).await);
        let err = c.cancel("ses_x").await.unwrap_err();
        assert!(matches!(err, AgentEngineError::SessionNotFound(ref id) if id == "ses_x"));
    }

    #[tokio::test]
    async fn network_error_does_not_leak_url() {
        // Bind then drop so the port refuses connections.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let c = client_for(format!("http://127.0.0.1:{port}"));
        let err = c.model_catalog().await.unwrap_err();
        assert!(matches!(err, AgentEngineError::Network(_)));
        assert!(!err.to_string().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn oversized_response_is_rejected() {
        let c = client_for(test_server::serve(200, vec![b' '; MAX_RESPONSE_BYTES + 1]).await);
        let err = c.model_catalog().await.unwrap_err();
        assert!(matches!(err, AgentEngineError::Protocol(_)));
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
    async fn prompt_rejects_attachments_and_unknown_delivery() {
        let c = client_for("http://127.0.0.1:1".to_string());
        let e = c
            .send_prompt("s", "hi", &["a.png".to_string()], None)
            .await
            .unwrap_err();
        assert!(matches!(e, AgentEngineError::Unsupported(_)));
        let e = c
            .send_prompt("s", "hi", &[], Some("bogus"))
            .await
            .unwrap_err();
        assert!(matches!(e, AgentEngineError::RequestFailed(_)));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_for_log("aé", 2), "a");
        assert_eq!(truncate_for_log("abc", 10), "abc");
    }
}
