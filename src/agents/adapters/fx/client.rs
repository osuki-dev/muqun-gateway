//! FX launch arguments and its exact, safe native error guidance.
use std::ops::Deref;
use std::sync::Arc;

use serde_json::Value;

use crate::agents::adapters::acp::AcpClient;
pub use crate::agents::adapters::acp::AcpEvent as FxEvent;

/// Keep FX's existing constructor while sharing all RPC and process ownership.
pub struct FxClient {
    inner: Arc<AcpClient>,
}

impl FxClient {
    pub async fn spawn(binary: &str, cwd: &str) -> Result<Arc<Self>, String> {
        let inner =
            AcpClient::spawn_with_error_mapper(binary, &["acp"], cwd, "FX", safe_error_message)
                .await?;
        Ok(Arc::new(Self { inner }))
    }
}

impl Deref for FxClient {
    type Target = AcpClient;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn safe_error_message(error: &Value) -> Option<&'static str> {
    // Exact FX 0.0.16 constants only. Never expose arbitrary native diagnostics.
    match error.get("message").and_then(Value::as_str) {
        Some("Session is busy") => Some("Session is busy"),
        Some("Session not found") => Some("Session not found"),
        Some("Prompt already in progress") => Some("Prompt already in progress"),
        Some("Subagent child sessions cannot be resumed directly") => Some("Subagent child sessions cannot be resumed directly"),
        Some("Configured provider authentication is unavailable") => Some("Configured provider authentication is unavailable"),
        Some("fx needs access to Vercel AI Gateway. Run fx login to sign in, fx setup to use an API key, or set AI_GATEWAY_API_KEY.") => Some("FX authentication is unavailable; run fx login or fx setup in the terminal"),
        Some("fx needs a Codex subscription login for this model. Run fx login codex.") => Some("FX Codex authentication is unavailable; run fx login codex in the terminal"),
        Some("fx needs a Grok subscription login for this model. Run fx login grok.") => Some("FX Grok authentication is unavailable; run fx login grok in the terminal"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn native_error_allowlist_requires_an_exact_safe_message() {
        assert_eq!(
            safe_error_message(&json!({"message":"Session is busy"})),
            Some("Session is busy")
        );
        assert_eq!(
            safe_error_message(&json!({"message":"Session is busy: secret"})),
            None
        );
        assert!(safe_error_message(&json!({"message":"fx needs a Codex subscription login for this model. Run fx login codex."})).unwrap().contains("run fx login codex"));
    }
    #[tokio::test]
    #[ignore = "requires explicit FX_ACP_TEST_BINARY; creates one isolated FX session without a model prompt"]
    async fn real_fx_initialize_and_new_without_prompt() {
        let binary =
            std::env::var("FX_ACP_TEST_BINARY").expect("explicit FX_ACP_TEST_BINARY required");
        let cwd = std::env::temp_dir().join(format!("muqun-fx-acp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&cwd).unwrap();
        let client = FxClient::spawn(&binary, cwd.to_str().unwrap())
            .await
            .unwrap();
        let _events = client.subscribe();
        let init = client.request_with_timeout("initialize", json!({
            "protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"muqun-gateway-transport-test","version":"test"}
        }), Duration::from_secs(30)).await.unwrap();
        assert_eq!(init["protocolVersion"], 1);
        let session = client
            .request_with_timeout(
                "session/new",
                json!({"cwd":cwd,"mcpServers":[]}),
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        assert!(session["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        client.shutdown().await;
        std::fs::remove_dir(cwd).unwrap();
    }
}
