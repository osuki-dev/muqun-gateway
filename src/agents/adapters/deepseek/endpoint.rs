use reqwest::Client;
use std::time::Duration;

/// The connection details for a DeepSeek Harness service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepseekEndpoint {
    pub url: String,
    pub ws_url: String,
    pub token: Option<String>,
    pub secret: Option<String>,
    pub version: Option<String>,
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

    /// Check if the DeepSeek Harness HTTP/RPC endpoint is reachable.
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
        match req.json(&body).send().await {
            Ok(resp) => {
                resp.status().is_success()
                    || resp.status().as_u16() == 401
                    || resp.status().as_u16() == 403
            }
            Err(_) => false,
        }
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
}
