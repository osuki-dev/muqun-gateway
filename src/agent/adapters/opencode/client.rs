use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

use super::discovery::OpencodeEndpoint;
use crate::agent::ports::engine::AgentEngineError;

#[derive(Clone)]
pub struct OpencodeClient {
    pub endpoint: OpencodeEndpoint,
    http: Client,
}

impl OpencodeClient {
    pub fn new(endpoint: OpencodeEndpoint) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { endpoint, http }
    }

    fn authed_req(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ref pwd) = self.endpoint.password {
            req = req.basic_auth("opencode", Some(pwd));
        }
        req
    }

    async fn handle_resp(&self, resp: Response) -> Result<Value, AgentEngineError> {
        let status = resp.status();
        if !status.is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(AgentEngineError::RequestFailed(format!(
                "HTTP {status}: {err_text}"
            )));
        }
        if status == StatusCode::NO_CONTENT {
            return Ok(json!({ "success": true }));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;
        if bytes.is_empty() {
            return Ok(json!({ "success": true }));
        }
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| AgentEngineError::Protocol(e.to_string()))
    }

    pub async fn get<K, V>(&self, path: &str, query: &[(K, V)]) -> Result<Value, AgentEngineError>
    where
        K: AsRef<str> + serde::Serialize,
        V: AsRef<str> + serde::Serialize,
    {
        let url = format!("{}{path}", self.endpoint.url);
        let req = self.authed_req(self.http.get(&url).query(query));
        let resp = req
            .send()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;
        self.handle_resp(resp).await
    }

    /// `get` with no query parameters.
    pub async fn get_plain(&self, path: &str) -> Result<Value, AgentEngineError> {
        self.get(path, &[] as &[(&str, &str)]).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, AgentEngineError> {
        let url = format!("{}{path}", self.endpoint.url);
        let req = self.authed_req(self.http.post(&url).json(body));
        let resp = req
            .send()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;
        self.handle_resp(resp).await
    }

    pub async fn list_projects(&self) -> Result<Vec<Value>, AgentEngineError> {
        let res = self.get_plain("/api/project").await?;
        if let Some(arr) = res.as_array() {
            return Ok(arr.clone());
        }
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn list_sessions(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let mut query = Vec::new();
        if let Some(d) = directory {
            query.push(("directory", d));
        }
        let res = self.get("/api/session", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn create_session(
        &self,
        directory: Option<&str>,
        model: Option<&crate::agent::domain::ModelRef>,
        agent: Option<&str>,
    ) -> Result<Value, AgentEngineError> {
        let mut body = json!({});
        if let Some(d) = directory {
            body["location"] = json!({ "directory": d });
        }
        // The `model` field is omitted when the caller did not pick one, so
        // OpenCode resolves the user's configured default itself. `Model.Ref`
        // declares `additionalProperties: false` with `variant` as a plain
        // string, so an explicit `variant: null` is never sent either.
        if let Some(m) = model {
            body["model"] = model_ref_json(m);
        }
        if let Some(a) = agent {
            body["agent"] = json!(a);
        }
        self.post("/api/session", &body).await
    }

    pub async fn get_session(&self, id: &str) -> Result<Value, AgentEngineError> {
        self.get_plain(&format!("/api/session/{id}")).await
    }

    pub async fn get_messages(&self, session_id: &str, limit: usize) -> Result<Vec<Value>, AgentEngineError> {
        let limit_str = limit.to_string();
        // `order` is sent explicitly rather than inferred from two timestamps.
        // It has to be `desc`: verified against 2.0.1, `limit` is applied from
        // the *start* of the requested order, so `order=asc&limit=100` returns
        // the first hundred messages of a long session instead of the hundred
        // the reader is looking at. The page is reversed here, so callers
        // always receive oldest-first.
        let res = self
            .get(
                &format!("/api/session/{session_id}/message"),
                &[("limit", limit_str.as_str()), ("order", "desc")],
            )
            .await?;
        let mut items = match res.as_array() {
            Some(arr) => arr.clone(),
            None => res
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        };
        items.reverse();
        Ok(items)
    }

    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<Value, AgentEngineError> {
        let mut body = json!({ "text": text });
        if !attachments.is_empty() {
            let files: Vec<Value> = attachments
                .iter()
                .map(|att| {
                    let name = std::path::Path::new(att)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("attachment");
                    let uri = if att.starts_with("file://")
                        || att.starts_with("http://")
                        || att.starts_with("https://")
                        || att.starts_with("data:")
                    {
                        att.clone()
                    } else {
                        format!("file://{att}")
                    };
                    json!({
                        "uri": uri,
                        "name": name,
                    })
                })
                .collect();
            body["files"] = json!(files);
        }
        if let Some(del) = delivery {
            if del == "steer" || del == "queue" {
                body["delivery"] = json!(del);
            }
        }
        self.post(&format!("/api/session/{session_id}/prompt"), &body)
            .await
    }

    pub async fn revert_session(
        &self,
        session_id: &str,
        message_id: &str,
    ) -> Result<Value, AgentEngineError> {
        let stage_body = json!({
            "messageID": message_id,
            "files": true,
        });
        self.post(&format!("/api/session/{session_id}/revert/stage"), &stage_body)
            .await?;
        self.post(&format!("/api/session/{session_id}/revert/commit"), &json!({}))
            .await
    }

    pub async fn interrupt(&self, session_id: &str) -> Result<Value, AgentEngineError> {
        self.post(&format!("/api/session/{session_id}/interrupt"), &json!({}))
            .await
    }

    pub async fn switch_model(
        &self,
        session_id: &str,
        model: &crate::agent::domain::ModelRef,
    ) -> Result<Value, AgentEngineError> {
        let body = json!({
            "model": model_ref_json(model),
        });
        self.post(&format!("/api/session/{session_id}/model"), &body)
            .await
    }

    pub async fn switch_agent(
        &self,
        session_id: &str,
        agent: &str,
    ) -> Result<Value, AgentEngineError> {
        let body = json!({
            "agent": agent,
        });
        self.post(&format!("/api/session/{session_id}/agent"), &body)
            .await
    }

    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        reply: &str,
    ) -> Result<Value, AgentEngineError> {
        let body = json!({
            "reply": reply,
        });
        self.post(
            &format!("/api/session/{session_id}/permission/{request_id}/reply"),
            &body,
        )
        .await
    }

    pub async fn reply_form(
        &self,
        session_id: &str,
        form_id: &str,
        answers: &Value,
    ) -> Result<Value, AgentEngineError> {
        let body = json!({
            "answer": answers,
        });
        self.post(
            &format!("/api/session/{session_id}/form/{form_id}/reply"),
            &body,
        )
        .await
    }

    pub async fn get_models(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let res = self.get("/api/model", &location_query(directory)).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_agents(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let res = self.get("/api/agent", &location_query(directory)).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_mcp(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let res = self.get("/api/mcp", &location_query(directory)).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// `GET /api/vcs/diff`. `mode` is a required parameter on this endpoint --
    /// omitting it is a 400, which is how this call used to come back empty.
    pub async fn get_vcs_diff(
        &self,
        directory: Option<&str>,
        mode: &str,
        base: Option<&str>,
    ) -> Result<Vec<Value>, AgentEngineError> {
        let mut query = location_query(directory);
        query.push(("mode".to_string(), mode.to_string()));
        if let Some(b) = base {
            query.push(("base".to_string(), b.to_string()));
        }
        let res = self.get("/api/vcs/diff", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_skills(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let res = self.get("/api/skill", &location_query(directory)).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// `GET /api/fs/find` (or `/api/fs/list` for an empty query). `/api/fs/list`
    /// takes `location` and `path` only -- it has no `limit`, so the cap is
    /// applied here instead of being sent and ignored.
    pub async fn find_files(
        &self,
        query: &str,
        limit: usize,
        directory: Option<&str>,
    ) -> Result<Vec<Value>, AgentEngineError> {
        if query.trim().is_empty() {
            let res = self.get("/api/fs/list", &location_query(directory)).await?;
            let mut items = res
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            items.truncate(limit);
            Ok(items)
        } else {
            let mut params = location_query(directory);
            params.push(("query".to_string(), query.to_string()));
            params.push(("limit".to_string(), limit.to_string()));
            let res = self.get("/api/fs/find", &params).await?;
            Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
        }
    }
}

/// Serialize a directory into the deepObject `location` parameter that every
/// catalog, filesystem and VCS endpoint in v2 declares. A flat `directory=` is
/// not a parameter those endpoints define, so it was silently ignored and the
/// catalog always resolved against the server's own cwd.
pub(crate) fn location_query(directory: Option<&str>) -> Vec<(String, String)> {
    match directory {
        Some(d) if !d.trim().is_empty() => {
            vec![("location[directory]".to_string(), d.to_string())]
        }
        _ => Vec::new(),
    }
}

/// `Model.Ref` as v2 spells it: `{providerID, id, variant?}`, with `variant`
/// omitted rather than sent as null.
pub(crate) fn model_ref_json(model: &crate::agent::domain::ModelRef) -> Value {
    let mut obj = json!({
        "providerID": model.provider_id,
        "id": model.model_id,
    });
    if let Some(ref v) = model.variant {
        obj["variant"] = json!(v);
    }
    obj
}
