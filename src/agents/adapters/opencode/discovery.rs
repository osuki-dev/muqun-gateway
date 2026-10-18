use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct OpencodeServiceRegistration {
    pub id: Option<String>,
    pub version: Option<String>,
    pub url: String,
    pub pid: Option<u32>,
    pub password: Option<String>,
}

#[derive(Clone)]
pub struct OpencodeEndpoint {
    pub url: String,
    pub password: Option<String>,
    pub version: Option<String>,
    /// The service's verified process id after probing. Kept so the
    /// gateway can report which binary an adopted service is actually running.
    pub pid: Option<u32>,
}

/// Identity fields from OpenCode 2.0.27's published `ServerInfo`. There is no
/// instance id in this response; the registration id is compared separately.
#[derive(Deserialize)]
pub struct OpencodeServerInfo {
    pub version: String,
    pub pid: u32,
    pub urls: Vec<String>,
}

impl std::fmt::Debug for OpencodeEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpencodeEndpoint")
            .field("url", &self.url)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("version", &self.version)
            .field("pid", &self.pid)
            .finish()
    }
}

impl std::fmt::Debug for OpencodeServiceRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpencodeServiceRegistration")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("version", &self.version)
            .field("pid", &self.pid)
            .finish()
    }
}

impl OpencodeEndpoint {
    /// Discover OpenCode service configuration from standard state files or environment.
    pub async fn discover() -> Option<Self> {
        Self::discover_with_registration_id()
            .await
            .map(|(endpoint, _)| endpoint)
    }

    /// Preserve the registration's instance id without inventing a server-info
    /// field. Environment endpoints have no registration identity to match.
    pub(crate) async fn discover_with_registration_id() -> Option<(Self, Option<String>)> {
        // 1. Check environment variables first if manually configured
        if let Ok(url) =
            std::env::var("OPENCODE_URL").or_else(|_| std::env::var("HERDR_GATEWAY_OPENCODE_URL"))
        {
            let url = url.trim().trim_end_matches('/').to_string();
            if !url.is_empty() {
                let password = std::env::var("OPENCODE_SERVER_PASSWORD")
                    .or_else(|_| std::env::var("OPENCODE_PASSWORD"))
                    .ok();
                return Some((
                    Self {
                        url,
                        password,
                        version: None,
                        pid: None,
                    },
                    None,
                ));
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

        Some((
            Self {
                url,
                password: reg.password,
                version: reg.version,
                pid: reg.pid,
            },
            reg.id,
        ))
    }

    /// Verify the current OpenCode service is running and responsive.
    pub async fn probe_healthy(&self, client: &reqwest::Client) -> bool {
        self.probe_info(client).await.is_some()
    }

    /// Authenticate and match the live identity against every supplied
    /// registration constraint. Do not fall back to the registration version.
    pub async fn probe_info(&self, client: &reqwest::Client) -> Option<OpencodeServerInfo> {
        let mut req = client.get(format!("{}/api/info", self.url));
        if let Some(ref pwd) = self.password {
            req = req.basic_auth("opencode", Some(pwd));
        }
        let resp = req.send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let info = resp.json::<OpencodeServerInfo>().await.ok()?;
        if info.version.trim().is_empty()
            || info.pid == 0
            || info.urls.is_empty()
            || info.urls.iter().any(|url| url.trim().is_empty())
            || self.pid.is_some_and(|pid| pid != info.pid)
            || self
                .version
                .as_ref()
                .is_some_and(|version| version != &info.version)
        {
            return None;
        }
        Some(info)
    }

    /// An endpoint suitable for attachment and status reporting, populated
    /// from authenticated live info rather than a possibly stale state file.
    pub async fn verified(&self, client: &reqwest::Client) -> Option<Self> {
        let info = self.probe_info(client).await?;
        Some(Self {
            version: Some(info.version),
            pid: Some(info.pid),
            ..self.clone()
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
        for info in [
            serde_json::json!({"version":" ","pid":123,"urls":["http://localhost"]}),
            serde_json::json!({"version":"2.0.27","pid":1.5,"urls":["http://localhost"]}),
            serde_json::json!({"version":"2.0.27","pid":4294967296u64,"urls":["http://localhost"]}),
            serde_json::json!({"version":"2.0.27","pid":123,"urls":[""]}),
            serde_json::json!({"version":"2.0.27","pid":123,"urls":[42]}),
        ] {
            assert!(!probe(StatusCode::OK, info).await);
        }
    }

    #[tokio::test]
    async fn matches_registration_identity_and_authenticates_info() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::get,
            Json, Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/api/info", get(|headers: HeaderMap| async move {
            if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Basic b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==") {
                return (StatusCode::UNAUTHORIZED, Json(serde_json::Value::Null));
            }
            (StatusCode::OK, Json(serde_json::json!({"version":"2.0.27","pid":123,"urls":["http://localhost"],"paths":{"tmp":"/fixture"}})))
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        let mut endpoint = OpencodeEndpoint {
            url,
            password: Some("fixture-password".into()),
            version: None,
            pid: None,
        };
        let verified = endpoint.verified(&client).await.unwrap();
        assert_eq!(verified.version.as_deref(), Some("2.0.27"));
        assert_eq!(verified.pid, Some(123));
        endpoint.pid = Some(456);
        assert!(endpoint.verified(&client).await.is_none());
        endpoint.pid = Some(123);
        endpoint.version = Some("2.0.26".into());
        assert!(endpoint.verified(&client).await.is_none());
        endpoint.version = Some("2.0.27".into());
        assert!(endpoint.verified(&client).await.is_some());
        endpoint.password = Some("wrong-fixture-password".into());
        assert!(endpoint.verified(&client).await.is_none());
        server.abort();
    }

    #[test]
    fn debug_redacts_service_passwords() {
        let reg: OpencodeServiceRegistration = serde_json::from_str(
            r#"{"url":"http://localhost","password":"fixture-private-password"}"#,
        )
        .unwrap();
        let endpoint = OpencodeEndpoint {
            url: reg.url.clone(),
            password: reg.password.clone(),
            version: None,
            pid: None,
        };
        for output in [format!("{reg:?}"), format!("{endpoint:?}")] {
            assert!(!output.contains("fixture-private-password"));
            assert!(output.contains("<redacted>"));
        }
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
