use reqwest::{Client, Response};
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
        resp.json::<Value>()
            .await
            .map_err(|e| AgentEngineError::Protocol(e.to_string()))
    }

    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, AgentEngineError> {
        let url = format!("{}{path}", self.endpoint.url);
        let req = self.authed_req(self.http.get(&url).query(query));
        let resp = req
            .send()
            .await
            .map_err(|e| AgentEngineError::Network(e.to_string()))?;
        self.handle_resp(resp).await
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
        if let Some(m) = model {
            body["model"] = json!({
                "providerID": m.provider_id,
                "id": m.model_id,
                "variant": m.variant
            });
        }
        if let Some(a) = agent {
            body["agent"] = json!(a);
        }
        self.post("/api/session", &body).await
    }

    pub async fn get_session(&self, id: &str) -> Result<Value, AgentEngineError> {
        self.get(&format!("/api/session/{id}"), &[]).await
    }

    pub async fn get_messages(&self, session_id: &str, limit: usize) -> Result<Vec<Value>, AgentEngineError> {
        let limit_str = limit.to_string();
        let res = self
            .get(
                &format!("/api/session/{session_id}/message"),
                &[("limit", &limit_str)],
            )
            .await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[String],
    ) -> Result<Value, AgentEngineError> {
        let mut body = json!({ "text": text });
        if !attachments.is_empty() {
            body["attachments"] = json!(attachments);
        }
        self.post(&format!("/api/session/{session_id}/prompt"), &body)
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
            "providerID": model.provider_id,
            "id": model.model_id,
            "variant": model.variant
        });
        self.post(&format!("/api/session/{session_id}/model"), &body)
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
        let mut query = Vec::new();
        if let Some(d) = directory {
            query.push(("directory", d));
        }
        let res = self.get("/api/model", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_agents(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let mut query = Vec::new();
        if let Some(d) = directory {
            query.push(("directory", d));
        }
        let res = self.get("/api/agent", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_mcp(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let mut query = Vec::new();
        if let Some(d) = directory {
            query.push(("directory", d));
        }
        let res = self.get("/api/mcp", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub async fn get_vcs_diff(&self, directory: Option<&str>) -> Result<Vec<Value>, AgentEngineError> {
        let mut query = Vec::new();
        if let Some(d) = directory {
            query.push(("directory", d));
        }
        let res = self.get("/api/vcs/diff", &query).await?;
        Ok(res.get("data").and_then(Value::as_array).cloned().unwrap_or_default())
    }
}
