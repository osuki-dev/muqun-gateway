//! Discover the installed GitHub Copilot CLI executable without creating an ACP session.
//! Each session's private `copilot --acp --stdio --no-auto-update` child belongs to the driver; external TUI
//! processes are never adopted or presented as a shared live session.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::ports::agent::{AgentError, AgentPort};
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo, AgentFeatures};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopilotConfig {
    #[serde(default)]
    pub enabled: bool,
    /// An absolute executable path, otherwise normal PATH lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

impl CopilotConfig {
    pub(crate) fn resolve_binary(&self) -> Result<PathBuf, AgentError> {
        if !self.enabled {
            return Err(AgentError::NotAvailable(
                "GitHub Copilot CLI is disabled".into(),
            ));
        }
        let path = match &self.binary {
            Some(binary) => {
                let path = PathBuf::from(binary);
                if !path.is_absolute() {
                    return Err(AgentError::InvalidRequest(
                        "copilot.binary must name an absolute executable path".into(),
                    ));
                }
                path
            }
            None => crate::agents::tasks::find_on_path("copilot").ok_or_else(|| {
                AgentError::NotAvailable(
                    "GitHub Copilot CLI executable was not found on PATH".into(),
                )
            })?,
        };
        if !super::is_executable(&path) {
            return Err(AgentError::NotAvailable(
                "GitHub Copilot CLI executable was not found or is not executable".into(),
            ));
        }
        Ok(path)
    }
}

pub(super) struct CopilotState {
    pub(super) config: CopilotConfig,
    retry_at: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
    start: tokio::sync::Mutex<()>,
    driver: std::sync::OnceLock<std::sync::Arc<crate::agents::adapters::copilot::CopilotDriver>>,
}

impl CopilotState {
    pub(super) fn new(config: CopilotConfig) -> Self {
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
        // A lazy GitHub Copilot CLI driver owns sessions independently: a failed session's child
        // must not discard the other live sessions or trigger prompt replay.
        if runtime.manager_for_agent("copilot").await.is_some() {
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
        let Some(version) = version(&binary).await else {
            return false;
        };
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let directory = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let (manager, driver) = crate::agents::manager::AgentManager::connect_copilot(
            binary.to_string_lossy().into_owned(),
            directory,
            Some(version),
            runtime.events_tx.clone(),
        );
        driver.observe_terminals(runtime.terminal_runtime.get().cloned());
        if let Some(servers) = runtime
            .acp_muqun_mcp_servers("copilot", "GitHub Copilot CLI")
            .await
        {
            driver.set_mcp_servers(servers);
        }
        let _ = self.driver.set(driver);
        runtime.install("copilot", manager, AgentOrigin::None).await;
        true
    }

    pub(super) async fn discovery(&self, runtime: &AgentRuntime) -> AgentDiscoveryInfo {
        let mut info = AgentDiscoveryInfo {
            id: "copilot".into(),
            name: "GitHub Copilot CLI".into(),
            kind: "copilot".into(),
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
        if let Some(manager) = runtime.manager_for_agent("copilot").await {
            // Reachable means the adapter can open ACP on demand. No live
            // process or authenticated model response is implied by discovery.
            info.status = AgentAvailability::Reachable;
            info.version = manager.version();
            if let Some(driver) = self.driver.get() {
                // Only observed/cached metadata: this provider has no global
                // catalog probe and this call never creates a new ACP session.
                if let Ok(catalog) = driver.available_catalog(None).await {
                    info.models = catalog
                        .models
                        .iter()
                        .map(crate::discovery::model_info_to_agent_model)
                        .collect();
                    info.modes = catalog
                        .modes
                        .iter()
                        .map(crate::discovery::mode_info_to_agent_mode)
                        .collect();
                    info.features.model_selection = !info.models.is_empty();
                    info.features.reasoning_effort = info
                        .models
                        .iter()
                        .any(|model| !model.reasoning_effort_tiers.is_empty());
                }
                super::apply_acp_capabilities(
                    &mut info.features,
                    &driver.observed_capabilities(),
                    driver.slash_commands_listed(),
                    driver.skills_listed(),
                );
            }
            return info;
        }
        match self.config.resolve_binary() {
            Ok(binary) => {
                info.version = version(&binary).await;
                info.status = if info.version.is_some() {
                    AgentAvailability::Reachable
                } else {
                    AgentAvailability::Offline
                };
                info.reason = Some(
                    "ACP capabilities and authentication are checked when opening a GitHub Copilot CLI session"
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
    pub(crate) async fn shutdown_copilot(&self) {
        self.copilot
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _start = self.copilot.start.lock().await;
        if let Some(manager) = self.manager_for_agent("copilot").await {
            manager.shutdown();
        }
        if let Some(driver) = self.copilot.driver.get() {
            driver.shutdown().await;
        }
    }
}

pub(super) fn features() -> AgentFeatures {
    let mut features = super::acp_features();
    features.modes = true;
    features
        .extra
        .insert("modeSwitching".into(), serde_json::json!(true));
    features
}

async fn version(binary: &std::path::Path) -> Option<String> {
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["--no-auto-update", "--version"])
        .env("MISE_AUTO_INSTALL", "false");
    super::binary_version_command(command)
        .await
        .filter(|version| version.starts_with("GitHub Copilot CLI "))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn opt_in_discovery_only_checks_version_and_never_installs_or_opens_a_session() {
        let directory =
            crate::test_state_dir().join(format!("copilot-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let binary = directory.join("copilot");
        let calls = directory.join("copilot.log");
        std::fs::write(
            &binary,
            r#"#!/bin/sh
[ "$MISE_AUTO_INSTALL" = false ] || exit 70
printf '%s\n' "$*" >> "$0.log"
[ "$*" = '--no-auto-update --version' ] || exit 71
printf 'GitHub Copilot CLI 1.0.114.\n'
"#,
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let make = |enabled| {
            AgentRuntime::with_local_configs(
                crate::agents::OpencodeConfig {
                    enabled: false,
                    ..Default::default()
                },
                crate::agents::DeepseekConfig::default(),
                crate::agents::T3Config::default(),
                super::super::LocalAgentConfigs {
                    copilot: CopilotConfig {
                        enabled,
                        binary: Some(binary.to_string_lossy().into_owned()),
                    },
                    ..Default::default()
                },
                None,
            )
        };
        let disabled = make(false);
        assert_eq!(
            disabled.copilot.discovery(&disabled).await.status,
            AgentAvailability::Disabled
        );
        assert!(!disabled.copilot.supervise(&disabled).await);
        assert!(!calls.exists());

        let runtime = make(true);
        assert_eq!(
            runtime.copilot.discovery(&runtime).await.status,
            AgentAvailability::Reachable
        );
        assert!(runtime.manager_for_agent("copilot").await.is_none());
        assert!(runtime.copilot.supervise(&runtime).await);
        let manager = runtime.manager_for_agent("copilot").await.unwrap();
        assert!(!manager.stream_connected());
        let _ = runtime.copilot.discovery(&runtime).await;
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
        runtime.shutdown_copilot().await;
        assert!(!runtime.copilot.supervise(&runtime).await);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
