use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::endpoint::DeepseekEndpoint;
use crate::agents::domain::{ModelRef, SessionQuery};
use crate::agents::ports::engine::AgentEngineError;

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

        let resp = req
            .json(&wire_payload)
            .send()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(AgentEngineError::RequestFailed(format!(
                "HTTP {status} calling {namespace}/{method}: {err_text}"
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;

        if bytes.is_empty() {
            return Ok(json!({ "success": true }));
        }

        let envelope: Value = serde_json::from_slice(&bytes)
            .map_err(|e| AgentEngineError::Protocol(format!("Invalid JSON RPC response: {e}")))?;

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

                if code.contains("not-found") {
                    Err(AgentEngineError::SessionNotFound(msg.to_string()))
                } else {
                    Err(AgentEngineError::RequestFailed(format!("{code}: {msg}")))
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
    ) -> Result<Value, AgentEngineError> {
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
        Ok(result)
    }

    /// Submit prompt to session: `session/prompt`
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        _attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<Value, AgentEngineError> {
        let req_id = format!("req_{}", Uuid::new_v4().simple());
        let content_parts = vec![json!({
            "type": "text",
            "text": text,
        })];

        let args = json!({
            "request": {
                "sessionId": session_id,
                "requestId": req_id,
                "mode": delivery.unwrap_or("steer"),
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
}
