//! Discover Google Antigravity's `agy` without starting it. Each native
//! session's private `agy acp` child belongs to the driver and starts only on
//! an explicit session operation; listing reads Antigravity's own index.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::ports::agent::{AgentError, AgentPort};
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo, AgentFeatures};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AntigravityConfig {
    #[serde(default = "super::default_true")]
    pub enabled: bool,
    /// An absolute executable path, otherwise normal PATH lookup of `agy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Release verified idle native children; background reads never extend it.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_idle_timeout_secs() -> u64 {
    crate::agents::adapters::acp::session::DEFAULT_IDLE_TIMEOUT_SECS
}

impl Default for AntigravityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

impl AntigravityConfig {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub(crate) fn resolve_binary(&self) -> Result<PathBuf, AgentError> {
        if !self.enabled {
            return Err(AgentError::NotAvailable("Antigravity is disabled".into()));
        }
        let path = match &self.binary {
            Some(binary) => {
                let path = PathBuf::from(binary);
                if !path.is_absolute() {
                    return Err(AgentError::InvalidRequest(
                        "antigravity.binary must name an absolute executable path".into(),
                    ));
                }
                path
            }
            None => crate::agents::tasks::find_on_path("agy").ok_or_else(|| {
                AgentError::NotAvailable("Antigravity's agy was not found on PATH".into())
            })?,
        };
        if !super::is_executable(&path) {
            return Err(AgentError::NotAvailable(
                "Antigravity's agy was not found or is not executable".into(),
            ));
        }
        Ok(path)
    }
}

pub(super) struct AntigravityState {
    pub(super) config: AntigravityConfig,
    retry_at: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
    start: tokio::sync::Mutex<()>,
    driver: std::sync::OnceLock<
        std::sync::Arc<crate::agents::adapters::antigravity::AntigravityDriver>,
    >,
}

impl AntigravityState {
    pub(super) fn new(config: AntigravityConfig) -> Self {
        Self {
            config,
            retry_at: tokio::sync::Mutex::new(None),
            stopped: std::sync::atomic::AtomicBool::new(false),
            start: tokio::sync::Mutex::new(()),
            driver: std::sync::OnceLock::new(),
        }
    }

    pub(super) async fn supervise(&self, runtime: &AgentRuntime) -> bool {
        let _start = self.start.lock().await;
        if !self.config.enabled || self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        if runtime.manager_for_agent("antigravity").await.is_some() {
            return true;
        }
        if self
            .retry_at
            .lock()
            .await
            .is_some_and(|at| tokio::time::Instant::now() < at)
        {
            return false;
        }
        *self.retry_at.lock().await = Some(tokio::time::Instant::now() + super::MAX_START_BACKOFF);
        let Ok(binary) = self.config.resolve_binary() else {
            return false;
        };
        // Diagnostic only; compatibility is ACP `initialize` per session.
        let Some(version) = super::binary_version(&binary).await else {
            return false;
        };
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let directory = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let (manager, driver) = crate::agents::manager::AgentManager::connect_antigravity(
            binary.to_string_lossy().into_owned(),
            directory,
            Some(version),
            runtime.events_tx.clone(),
        );
        driver.set_idle_timeout(std::time::Duration::from_secs(
            self.config.idle_timeout_secs.max(1),
        ));
        if let Some(servers) = runtime.antigravity_mcp_servers().await {
            driver.set_mcp_servers(servers);
        }
        let _ = self.driver.set(driver);
        runtime
            .install("antigravity", manager, AgentOrigin::None)
            .await;
        true
    }

    pub(super) async fn discovery(&self, runtime: &AgentRuntime) -> AgentDiscoveryInfo {
        let mut info = AgentDiscoveryInfo {
            id: "antigravity".into(),
            name: "Antigravity".into(),
            kind: "antigravity".into(),
            status: AgentAvailability::Disabled,
            enabled: self.config.enabled,
            endpoint: None,
            version: None,
            reason: None,
            models: Vec::new(),
            modes: Vec::new(),
            features: features(),
        };
        if !self.config.enabled {
            return info;
        }
        if let Some(manager) = runtime.manager_for_agent("antigravity").await {
            // Reachable means a session can be opened on demand; the Google
            // login is checked by ACP `authenticate` when one is.
            info.status = AgentAvailability::Reachable;
            info.version = manager.version();
            if let Some(driver) = self.driver.get() {
                if let Ok(catalog) = driver.available_catalog(None).await {
                    info.models = catalog
                        .models
                        .iter()
                        .map(crate::discovery::model_info_to_agent_model)
                        .collect();
                    info.features = features_for_models(&info.models);
                }
                super::apply_acp_capabilities(
                    &mut info.features,
                    &driver.observed_capabilities(),
                    driver.slash_commands_listed(),
                );
            }
            return info;
        }
        match self.config.resolve_binary() {
            Ok(binary) => {
                info.version = super::binary_version(&binary).await;
                info.status = if info.version.is_some() {
                    AgentAvailability::Reachable
                } else {
                    AgentAvailability::Offline
                };
                info.reason = Some(
                    "The Google login is checked when an Antigravity session is opened".into(),
                );
            }
            Err(error) => {
                info.status = if self.config.binary.is_some() {
                    AgentAvailability::Unconfigured
                } else {
                    AgentAvailability::NotInstalled
                };
                info.reason = Some(error.to_string());
            }
        }
        info
    }
}

impl AgentRuntime {
    pub(crate) async fn shutdown_antigravity(&self) {
        self.antigravity
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _start = self.antigravity.start.lock().await;
        if let Some(manager) = self.manager_for_agent("antigravity").await {
            manager.shutdown();
        }
        if let Some(driver) = self.antigravity.driver.get() {
            driver.shutdown().await;
        }
    }

    /// The muqun server as an ACP `mcpServers` entry with a token bound to
    /// Antigravity's native sessions, or `None` when MCP is not wired or the
    /// token could not be minted. Earlier such tokens belonged to children of
    /// a previous Gateway run and are revoked.
    async fn antigravity_mcp_servers(&self) -> Option<serde_json::Value> {
        let wiring = self.mcp.get()?;
        match super::issue_agent_env_for(
            wiring.tokens.as_ref(),
            &wiring.url,
            "antigravity",
            "antigravity (native sessions)",
        )
        .await
        {
            Ok((env, previous)) => {
                super::settle_agent_tokens(wiring.tokens.as_ref(), &env, &previous, true).await;
                Some(serde_json::json!([{
                    "type": "http",
                    "name": crate::mcp::hosts::SERVER_NAME,
                    "url": env.url,
                    "headers": [{ "name": "Authorization", "value": format!("Bearer {}", env.token) }],
                }]))
            }
            Err(error) => {
                tracing::warn!("could not mint Antigravity's MCP token: {error}");
                None
            }
        }
    }
}

fn features_for_models(models: &[crate::discovery::AgentModelInfo]) -> AgentFeatures {
    let mut found = features();
    found.model_selection = !models.is_empty();
    found.reasoning_effort = models
        .iter()
        .any(|model| !model.reasoning_effort_tiers.is_empty());
    found
}

pub(super) fn features() -> AgentFeatures {
    super::acp_features()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_omitted_and_bad_values_refused() {
        let config: AntigravityConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, AntigravityConfig::default());
        assert!(config.enabled);
        assert_eq!(config.idle_timeout_secs, 3600);
        assert!(serde_json::from_str::<AntigravityConfig>(r#"{"token":"secret"}"#).is_err());
        let relative = AntigravityConfig {
            binary: Some("agy --unsafe".into()),
            ..AntigravityConfig::default()
        };
        assert!(matches!(
            relative.resolve_binary(),
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(matches!(
            AntigravityConfig::disabled().resolve_binary(),
            Err(AgentError::NotAvailable(_))
        ));
    }

    #[tokio::test]
    async fn discovery_names_each_unavailable_state() {
        let missing = std::env::temp_dir().join(format!("muqun-no-agy-{}", uuid::Uuid::new_v4()));
        let runtime = AgentRuntime::disabled();
        for (config, status) in [
            (AntigravityConfig::disabled(), AgentAvailability::Disabled),
            (
                AntigravityConfig {
                    binary: Some(missing.to_string_lossy().into_owned()),
                    ..AntigravityConfig::default()
                },
                AgentAvailability::Unconfigured,
            ),
        ] {
            let info = AntigravityState::new(config).discovery(&runtime).await;
            assert_eq!(info.kind, "antigravity");
            assert_eq!(info.status, status);
        }
    }
}
