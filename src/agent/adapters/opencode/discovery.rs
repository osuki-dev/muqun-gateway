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

    /// Health check probe to verify the service is running and responsive
    pub async fn probe_healthy(&self, client: &reqwest::Client) -> bool {
        let health_url = format!("{}/api/health", self.url);
        let mut req = client.get(&health_url);
        if let Some(ref pwd) = self.password {
            req = req.basic_auth("opencode", Some(pwd));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    body.get("healthy").and_then(|v| v.as_bool()) == Some(true)
                } else {
                    false
                }
            }
            _ => false,
        }
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

    #[test]
    fn test_service_registration_parse() {
        let json = r#"{"id":"test-id","version":"2.0.1","url":"http://127.0.0.1:4096","pid":123,"password":"secret"}"#;
        let reg: OpencodeServiceRegistration = serde_json::from_str(json).unwrap();
        assert_eq!(reg.version.as_deref(), Some("2.0.1"));
        assert_eq!(reg.url, "http://127.0.0.1:4096");
        assert_eq!(reg.password.as_deref(), Some("secret"));
    }
}
