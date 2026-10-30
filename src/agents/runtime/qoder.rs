//! Discover the installed Qoder CLI executable without creating an ACP session.
//! Each session's private `qodercli --acp` child belongs to the driver; external TUI
//! processes are never adopted or presented as a shared live session.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::ports::agent::{AgentError, AgentPort};
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo, AgentFeatures};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QoderConfig {
    #[serde(default = "super::default_true")]
    pub enabled: bool,
    /// An absolute executable path, otherwise normal PATH lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Release verified idle native children; background reads never extend it.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_idle_timeout_secs() -> u64 {
    crate::agents::adapters::acp::session::DEFAULT_IDLE_TIMEOUT_SECS
}

impl Default for QoderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

impl QoderConfig {
    pub(crate) fn resolve_binary(&self) -> Result<PathBuf, AgentError> {
        if !self.enabled {
            return Err(AgentError::NotAvailable("Qoder CLI is disabled".into()));
        }
        let path = match &self.binary {
            Some(binary) => {
                let path = PathBuf::from(binary);
                if !path.is_absolute() {
                    return Err(AgentError::InvalidRequest(
                        "qoder.binary must name an absolute executable path".into(),
                    ));
                }
                path
            }
            None => crate::agents::tasks::find_on_path("qodercli").ok_or_else(|| {
                AgentError::NotAvailable("Qoder CLI executable was not found on PATH".into())
            })?,
        };
        if !super::is_executable(&path) {
            return Err(AgentError::NotAvailable(
                "Qoder CLI executable was not found or is not executable".into(),
            ));
        }
        Ok(path)
    }
}

pub(super) struct QoderState {
    pub(super) config: QoderConfig,
    retry_at: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
    start: tokio::sync::Mutex<()>,
    driver: std::sync::OnceLock<std::sync::Arc<crate::agents::adapters::qoder::QoderDriver>>,
}

impl QoderState {
    pub(super) fn new(config: QoderConfig) -> Self {
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
        // A lazy Qoder CLI driver owns sessions independently: a failed session's child
        // must not discard the other live sessions or trigger prompt replay.
        if runtime.manager_for_agent("qoder").await.is_some() {
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
        let Some(version) = super::binary_version(&binary).await else {
            return false;
        };
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let directory = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let (manager, driver) = crate::agents::manager::AgentManager::connect_qoder(
            binary.to_string_lossy().into_owned(),
            directory,
            Some(version),
            runtime.events_tx.clone(),
        );
        driver.set_idle_timeout(std::time::Duration::from_secs(
            self.config.idle_timeout_secs.max(1),
        ));
        let _ = self.driver.set(driver);
        runtime.install("qoder", manager, AgentOrigin::None).await;
        true
    }

    pub(super) async fn discovery(&self, runtime: &AgentRuntime) -> AgentDiscoveryInfo {
        let mut info = AgentDiscoveryInfo {
            id: "qoder".into(),
            name: "Qoder CLI".into(),
            kind: "qoder".into(),
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
        if let Some(manager) = runtime.manager_for_agent("qoder").await {
            // Reachable means the adapter can open ACP on demand. No live
            // process or authenticated model response is implied by discovery.
            info.status = AgentAvailability::Reachable;
            info.version = manager.version();
            if let Some(driver) = self.driver.get() {
                match driver.available_catalog(None).await {
                    Ok(catalog) => {
                        info.models = catalog
                            .models
                            .iter()
                            .map(crate::discovery::model_info_to_agent_model)
                            .collect();
                        info.features = features_for_models(&info.models);
                    }
                    Err(_) => {
                        info.reason = Some("Qoder model catalog is unavailable; check CLI configuration on this host".into());
                    }
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
                    "ACP capabilities and authentication are checked when opening an Qoder CLI session"
                        .into(),
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
    pub(crate) async fn shutdown_qoder(&self) {
        self.qoder
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _start = self.qoder.start.lock().await;
        if let Some(manager) = self.manager_for_agent("qoder").await {
            manager.shutdown();
        }
        if let Some(driver) = self.qoder.driver.get() {
            driver.shutdown().await;
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
    fn config_defaults_and_invalid_paths_are_explicit() {
        let config: QoderConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, QoderConfig::default());
        let config = QoderConfig {
            binary: Some("qoder --unsafe".into()),
            ..config
        };
        assert!(matches!(
            config.resolve_binary(),
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(serde_json::from_str::<QoderConfig>(r#"{"token":"secret"}"#).is_err());
    }

    #[test]
    fn every_app_control_is_declared_including_optional_extras() {
        let value = serde_json::to_value(features()).unwrap();
        for key in [
            "reasoningEffort",
            "modelSelection",
            "worktrees",
            "revert",
            "stagedRevert",
            "inbox",
            "modes",
            "skills",
            "slashCommands",
            "compaction",
            "backgroundShells",
            "attachments",
            "attachmentsByPath",
            "inboxEdit",
            "modeSwitching",
            "sessionRename",
            "sessionDelete",
            "sessionFork",
            "sessionPagination",
            "accountLimits",
            "computerTools",
        ] {
            assert_eq!(value[key], false, "{key}");
        }
        assert_eq!(value["timelineEarlier"], true);
    }
}
