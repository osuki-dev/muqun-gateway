use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeServiceRegistration {
    pub id: Option<String>,
    pub version: Option<String>,
    pub url: String,
    pub pid: Option<u32>,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpencodeEndpoint {
    pub url: String,
    pub password: Option<String>,
    pub version: Option<String>,
    /// The service's process id, when its registration named one. Kept so the
    /// gateway can report which binary an adopted service is actually running.
    pub pid: Option<u32>,
}

impl OpencodeEndpoint {
    /// Discover OpenCode service configuration from standard state files or environment.
    pub async fn discover() -> Option<Self> {
        // 1. Check environment variables first if manually configured
        if let Ok(url) =
            std::env::var("OPENCODE_URL").or_else(|_| std::env::var("HERDR_GATEWAY_OPENCODE_URL"))
        {
            let url = url.trim().trim_end_matches('/').to_string();
            if !url.is_empty() {
                let password = std::env::var("OPENCODE_SERVER_PASSWORD")
                    .or_else(|_| std::env::var("OPENCODE_PASSWORD"))
                    .ok();
                return Some(Self {
                    url,
                    password,
                    version: None,
                    pid: None,
                });
            }
        }

        // 2. Discover from standard service.json registration file
        let reg_path = service_registration_path()?;
        if !reg_path.exists() {
            return None;
        }

        let content = tokio::fs::read_to_string(&reg_path).await.ok()?;
        let reg: OpencodeServiceRegistration = serde_json::from_str(&content).ok()?;
        let url = reg.url.trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            return None;
        }

        Some(Self {
            url,
            password: reg.password,
            version: reg.version,
            pid: reg.pid,
        })
    }

    /// Verify the current OpenCode service is running and responsive.
    pub async fn probe_healthy(&self, client: &reqwest::Client) -> bool {
        let mut req = client.get(format!("{}/api/info", self.url));
        if let Some(ref pwd) = self.password {
            req = req.basic_auth("opencode", Some(pwd));
        }
        let Ok(resp) = req.send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        let Ok(body) = resp.json::<serde_json::Value>().await else {
            return false;
        };
        body.get("version")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty())
            && body
                .get("pid")
                .and_then(|v| v.as_u64())
                .is_some_and(|pid| pid > 0)
            && body
                .get("urls")
                .and_then(|v| v.as_array())
                .is_some_and(|urls| {
                    !urls.is_empty() && urls.iter().all(|url| url.as_str().is_some())
                })
    }
}

fn service_registration_path() -> Option<PathBuf> {
    if let Ok(state_home) = std::env::var("XDG_STATE_HOME") {
        let p = Path::new(&state_home).join("opencode/service.json");
        return Some(p);
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/opencode/service.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn probe(info_status: axum::http::StatusCode, info: serde_json::Value) -> bool {
        use axum::{routing::get, Json, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = OpencodeEndpoint {
            url: format!("http://{}", listener.local_addr().unwrap()),
            password: None,
            version: None,
            pid: None,
        };
        let app = Router::new().route(
            "/api/info",
            get(move || async move { (info_status, Json(info)) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let healthy = endpoint.probe_healthy(&client).await;
        server.abort();
        healthy
    }

    #[tokio::test]
    async fn requires_current_server_info() {
        use axum::http::StatusCode;
        assert!(
            probe(
                StatusCode::OK,
                serde_json::json!({"version":"2.0.12","pid":123,"urls":["http://127.0.0.1:4096"]})
            )
            .await
        );
        assert!(!probe(StatusCode::NOT_FOUND, serde_json::Value::Null).await);
    }

    #[tokio::test]
    async fn does_not_hide_failed_auth_or_accept_an_unrelated_json_response() {
        use axum::http::StatusCode;
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(!probe(status, serde_json::Value::Null).await);
        }
        assert!(!probe(StatusCode::OK, serde_json::json!({"healthy":true})).await);
        assert!(
            !probe(
                StatusCode::OK,
                serde_json::json!({"version":"2.0.12","pid":0,"urls":[]})
            )
            .await
        );
    }

    #[test]
    fn test_service_registration_parse() {
        let json = r#"{"id":"test-id","version":"2.0.1","url":"http://127.0.0.1:4096","pid":123,"password":"secret"}"#;
        let reg: OpencodeServiceRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.version.as_deref(), Some("2.0.1"));
        assert_eq!(reg.url, "http://127.0.0.1:4096");
        assert_eq!(reg.password.as_deref(), Some("secret"));
    }
}
