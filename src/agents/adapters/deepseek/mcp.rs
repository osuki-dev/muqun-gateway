//! DSH loads the official MCP client once in the host composition. Its global
//! tools are inherited by Agent scopes; no session installs another server.
//! Registration uses the supported launcher overlay, not an invented RPC.

use serde_json::{json, Value};

use super::client::DeepseekClient;
use crate::agents::ports::agent::AgentError;

pub(crate) const ENTRY_ID: &str = "include:muqun-gateway-mcp";
pub(crate) const MODULE: &str = "@deepseek-ai/dsh-mcp-client";

/// No credentials are persisted or passed in argv. DSH evaluates these
/// expressions in the child environment supplied by the Gateway.
pub(crate) const OVERLAY: &str = r#"- insert:
    - id: muqun-gateway-mcp
      name: '@deepseek-ai/dsh-mcp-client'
      config:
        serverName: muqun
        transport: streamable-http
        url: !!js process.env.MUQUN_MCP_URL
        headers:
          Authorization: !!js '`Bearer ${process.env.MUQUN_MCP_TOKEN}`'
        failOnStartupError: true
"#;

pub(crate) fn active(value: &Value) -> bool {
    let Some(entries) = value.get("entries").and_then(Value::as_array) else {
        return false;
    };
    let mut matches = entries
        .iter()
        .filter(|entry| entry.get("entryId").and_then(Value::as_str) == Some(ENTRY_ID));
    let Some(entry) = matches.next() else {
        return false;
    };
    matches.next().is_none()
        && entry.get("moduleName").and_then(Value::as_str) == Some(MODULE)
        && entry.get("enabled").and_then(Value::as_bool) == Some(true)
        && entry.get("fiberPhase").and_then(Value::as_str) == Some("active")
}

pub(crate) async fn connected(client: &DeepseekClient) -> Result<bool, AgentError> {
    let inventory = client
        .call_remote("pluginInventory", "list", json!({}))
        .await?;
    Ok(active(&inventory))
}

pub(crate) fn unavailable() -> AgentError {
    AgentError::NotAvailable(
        "DeepSeek computer tools are not loaded. Restart DeepSeek through Gateway with MCP injection enabled; the existing conversation has not been sent or restarted.".into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Value {
        json!({"entryId": ENTRY_ID, "moduleName": MODULE, "enabled": true, "fiberPhase": "active"})
    }

    #[test]
    fn readiness_requires_the_unique_enabled_official_entry() {
        let good = entry();
        assert!(active(&json!({"entries": [good.clone()]})));
        for (key, value) in [
            ("enabled", json!(false)),
            ("fiberPhase", json!("loading")),
            ("fiberPhase", json!("failed")),
            ("moduleName", json!("another-plugin")),
            ("entryId", json!("another-server")),
        ] {
            let mut bad = good.clone();
            bad[key] = value;
            assert!(!active(&json!({"entries": [bad]})));
        }
        assert!(!active(&json!({"entries": [good.clone(), good]})));
        assert!(!active(&json!({"agentPresets": [{"rows": [entry()]}]})));
        assert!(!active(&json!({"entries": null})));
    }

    #[tokio::test]
    async fn readiness_uses_the_public_inventory_without_sending_a_prompt() {
        use axum::{routing::post, Json, Router};
        let app = Router::new().route(
            "/api/pluginInventory/list",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["method"], "pluginInventory/list");
                assert_eq!(body["payload"]["args"], json!({}));
                Json(json!({"result": {"ok": true, "value": {"entries": [entry()]}}}))
            }),
        );
        let (client, server) = super::super::client::tests::rpc_server(app).await;
        assert!(connected(&client).await.unwrap());
        assert!(connected(&client).await.unwrap());
        server.abort();
    }
}
