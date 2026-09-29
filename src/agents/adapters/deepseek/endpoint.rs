use reqwest::Client;
use std::time::Duration;

/// The connection details for a DeepSeek Harness service instance.
#[derive(Clone, PartialEq, Eq)]
pub struct DeepseekEndpoint {
    pub url: String,
    pub ws_url: String,
    pub token: Option<String>,
    pub secret: Option<String>,
    pub version: Option<String>,
}

impl std::fmt::Debug for DeepseekEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        f.debug_struct("DeepseekEndpoint")
            .field("url", &self.url)
            .field("ws_url", &self.ws_url)
            .field("token", &redact(&self.token))
            .field("secret", &redact(&self.secret))
            .field("version", &self.version)
            .finish()
    }
}

impl DeepseekEndpoint {
    pub fn new(url: impl Into<String>, token: Option<String>, secret: Option<String>) -> Self {
        let raw_url = url.into();
        let trimmed_url = raw_url.trim_end_matches('/').to_string();
        let ws_url = if let Some(host) = trimmed_url.strip_prefix("https://") {
            format!("wss://{host}/api/remote.mux")
        } else if let Some(host) = trimmed_url.strip_prefix("http://") {
            format!("ws://{host}/api/remote.mux")
        } else {
            format!("ws://{trimmed_url}/api/remote.mux")
        };

        Self {
            url: trimmed_url,
            ws_url,
            token,
            secret,
            version: None,
        }
    }

    /// Extract the HTTP Host/Authority header value (e.g. "127.0.0.1:3080").
    pub fn authority(&self) -> String {
        let u = self
            .url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        u.split('/').next().unwrap_or(u).to_string()
    }

    /// Compute the authority-bound signed session cookie if a secret is available.
    pub fn auth_cookie(&self) -> Option<String> {
        if let Some(ref sec) = self.secret {
            super::auth::generate_dsh_cookie(&self.authority(), sec)
                .ok()
                .map(|(n, v)| format!("{n}={v}"))
        } else {
            None
        }
    }

    /// Check that a DeepSeek Harness answers RPC with our credentials: a 2xx
    /// whose body is a Typert `server-response` envelope. Anything else
    /// (401/403 included) means we cannot use the service.
    pub async fn probe_healthy(&self, client: &Client) -> bool {
        let probe_url = format!("{}/api/session/modelCatalog", self.url);
        let mut req = client.post(&probe_url).timeout(Duration::from_millis(1500));
        if let Some(cookie) = self.auth_cookie() {
            req = req.header("Cookie", cookie);
        }
        req = req.header("Host", self.authority());
        if let Some(ref token) = self.token {
            req = req.bearer_auth(token);
        }
        let body = serde_json::json!({
            "type": "client-request",
            "rpcId": "probe",
            "method": "session/modelCatalog",
            "payload": { "args": {} }
        });
        let Ok(resp) = req.json(&body).send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        let Ok(bytes) = super::client::read_capped(resp, 64 * 1024).await else {
            return false;
        };
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "server-response")
            })
            .unwrap_or(false)
    }

    /// Discover a running DeepSeek Harness instance from environment or default ports.
    pub async fn discover() -> Option<Self> {
        let candidate_urls = if let Ok(env_url) = std::env::var("DEEPSEEK_HARNESS_URL") {
            vec![env_url]
        } else if let Ok(env_url) = std::env::var("DSH_URL") {
            vec![env_url]
        } else {
            vec![
                "http://127.0.0.1:3080".to_string(),
                "http://localhost:3080".to_string(),
                "http://127.0.0.1:19387".to_string(),
                "http://localhost:19387".to_string(),
            ]
        };

        let token = std::env::var("DEEPSEEK_HARNESS_TOKEN")
            .or_else(|_| std::env::var("DSH_TOKEN"))
            .ok();

        let secret = std::env::var("DEEPSEEK_HARNESS_SECRET")
            .or_else(|_| std::env::var("DSH_SECRET"))
            .ok()
            .or_else(super::auth::load_local_secret);

        let client = Client::builder()
            .timeout(Duration::from_millis(800))
            .build()
            .unwrap_or_default();

        for url in candidate_urls {
            let endpoint = Self::new(url, token.clone(), secret.clone());
            if endpoint.probe_healthy(&client).await {
                return Some(endpoint);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ws_url_and_authority_correctly() {
        let ep = DeepseekEndpoint::new("http://127.0.0.1:19387", Some("test-tok".into()), None);
        assert_eq!(ep.url, "http://127.0.0.1:19387");
        assert_eq!(ep.ws_url, "ws://127.0.0.1:19387/api/remote.mux");
        assert_eq!(ep.authority(), "127.0.0.1:19387");
        assert_eq!(ep.token.as_deref(), Some("test-tok"));

        let ep_ssl = DeepseekEndpoint::new("https://dsh.example.com", None, None);
        assert_eq!(ep_ssl.ws_url, "wss://dsh.example.com/api/remote.mux");
        assert_eq!(ep_ssl.authority(), "dsh.example.com");
    }

    #[test]
    fn generates_auth_cookie_when_secret_provided() {
        let secret = "Qz1iQUfg3Hve5G6HgLLfie-xeSvi016I1X24SBL-WC8".to_string();
        let ep = DeepseekEndpoint::new("http://127.0.0.1:3080", None, Some(secret));
        let cookie = ep.auth_cookie();
        assert!(cookie.is_some());
        let c = cookie.unwrap();
        assert!(c.starts_with("dsh-auth-VPhEEcLKeqRDBoBalzN2Nm7CnfxKhLE00pKIDWxt1sw=v1."));
    }

    #[test]
    fn debug_redacts_token_and_secret() {
        let ep = DeepseekEndpoint::new("http://h", Some("tok-123".into()), Some("sec-456".into()));
        let dbg = format!("{ep:?}");
        assert!(!dbg.contains("tok-123") && !dbg.contains("sec-456"));
        assert!(dbg.contains("<redacted>"));
        let none = format!("{:?}", DeepseekEndpoint::new("http://h", None, None));
        assert!(none.contains("token: None"));
    }

    async fn probe(status: u16, body: &str) -> bool {
        let url = crate::agents::adapters::deepseek::client::test_server::serve(
            status,
            body.as_bytes().to_vec(),
        )
        .await;
        DeepseekEndpoint::new(url, None, None)
            .probe_healthy(&Client::new())
            .await
    }

    #[tokio::test]
    async fn probe_accepts_only_2xx_server_response() {
        assert!(probe(200, r#"{"type":"server-response","rpcId":"probe"}"#).await);
        assert!(!probe(200, "<html>hello</html>").await);
        assert!(!probe(200, r#"{"type":"other"}"#).await);
        assert!(!probe(401, r#"{"type":"server-response"}"#).await);
        assert!(!probe(403, "").await);
        assert!(!probe(500, "").await);
    }
}
