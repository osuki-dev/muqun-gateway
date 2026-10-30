//! Discover the installed FX executable without creating an ACP session.
//! Each session's private `fx acp` child belongs to the driver; external TUI
//! processes are never adopted or presented as a shared live session.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::ports::agent::AgentError;
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo, AgentFeatures};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FxConfig {
    #[serde(default = "super::default_true")]
    pub enabled: bool,
    /// An absolute executable path, otherwise normal PATH lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
        }
    }
}

impl FxConfig {
    pub(crate) fn resolve_binary(&self) -> Result<PathBuf, AgentError> {
        if !self.enabled {
            return Err(AgentError::NotAvailable("FX is disabled".into()));
        }
        let path = match &self.binary {
            Some(binary) => {
                let path = PathBuf::from(binary);
                if !path.is_absolute() {
                    return Err(AgentError::InvalidRequest(
                        "fx.binary must name an absolute executable path".into(),
                    ));
                }
                path
            }
            None => crate::agents::tasks::find_on_path("fx").ok_or_else(|| {
                AgentError::NotAvailable("FX executable was not found on PATH".into())
            })?,
        };
        if !super::is_executable(&path) {
            return Err(AgentError::NotAvailable(
                "FX executable was not found or is not executable".into(),
            ));
        }
        Ok(path)
    }
}

pub(super) struct FxState {
    pub(super) config: FxConfig,
    retry_at: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
    start: tokio::sync::Mutex<()>,
    driver: std::sync::OnceLock<std::sync::Arc<crate::agents::adapters::fx::FxDriver>>,
}

impl FxState {
    pub(super) fn new(config: FxConfig) -> Self {
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
        // A lazy FX driver owns sessions independently: a failed session's child
        // must not discard the other live sessions or trigger prompt replay.
        if runtime.manager_for_agent("fx").await.is_some() {
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
        let (manager, driver) = crate::agents::manager::AgentManager::connect_fx(
            binary.to_string_lossy().into_owned(),
            directory,
            Some(version),
            runtime.events_tx.clone(),
        );
        let _ = self.driver.set(driver);
        runtime.install("fx", manager, AgentOrigin::None).await;
        true
    }

    pub(super) async fn discovery(&self, runtime: &AgentRuntime) -> AgentDiscoveryInfo {
        let mut info = AgentDiscoveryInfo {
            id: "fx".into(),
            name: "FX".into(),
            kind: "fx".into(),
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
        if let Some(manager) = runtime.manager_for_agent("fx").await {
            // Reachable means the adapter can open ACP on demand. No live
            // process or authenticated model response is implied by discovery.
            info.status = AgentAvailability::Reachable;
            info.version = manager.version();
            if let Some(driver) = self.driver.get() {
                info.models = driver
                    .observed_catalog()
                    .await
                    .models
                    .iter()
                    .map(crate::discovery::model_info_to_agent_model)
                    .collect();
                info.features = features_for_models(&info.models);
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
                    "ACP capabilities and authentication are checked when opening an FX session"
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
    pub(crate) async fn shutdown_fx(&self) {
        self.fx
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _start = self.fx.start.lock().await;
        if let Some(manager) = self.manager_for_agent("fx").await {
            manager.shutdown();
        }
        if let Some(driver) = self.fx.driver.get() {
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
        let config: FxConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, FxConfig::default());
        let config = FxConfig {
            binary: Some("fx --unsafe".into()),
            ..config
        };
        assert!(matches!(
            config.resolve_binary(),
            Err(AgentError::InvalidRequest(_))
        ));
        assert!(serde_json::from_str::<FxConfig>(r#"{"token":"secret"}"#).is_err());
    }

    #[test]
    fn optional_model_controls_require_observed_native_options() {
        let model = crate::discovery::AgentModelInfo {
            id: "native-model".into(),
            name: "Native model".into(),
            provider_id: "native-provider".into(),
            supports_reasoning: false,
            reasoning_effort_tiers: Vec::new(),
            extra: Default::default(),
        };
        let without_effort = features_for_models(std::slice::from_ref(&model));
        assert!(without_effort.model_selection);
        assert!(!without_effort.reasoning_effort);
        let with_effort = crate::discovery::AgentModelInfo {
            supports_reasoning: true,
            reasoning_effort_tiers: vec!["high".into()],
            ..model
        };
        assert!(features_for_models(&[with_effort]).reasoning_effort);
        let unobserved = features_for_models(&[]);
        assert!(!unobserved.model_selection && !unobserved.reasoning_effort);
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
            "timelineEarlier",
            "accountLimits",
            "computerTools",
        ] {
            assert_eq!(value[key], false, "{key}");
        }
    }
}
