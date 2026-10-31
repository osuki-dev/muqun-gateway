//! Claude Code as a native ACP agent through its official adapter,
//! `@agentclientprotocol/claude-agent-acp`. Claude Code itself does not
//! speak ACP. Startup only probes `claude` and Node.js; the adapter starts on
//! the first session open, create or prompt and reuses Claude Code's login.
//! Listing sessions reads Claude Code's transcript store and starts nothing.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{AgentOrigin, AgentRuntime};
use crate::agents::adapters::acp::session::AcpLaunch;
use crate::agents::ports::agent::AgentPort;
use crate::discovery::{AgentAvailability, AgentDiscoveryInfo, AgentFeatures};

pub(crate) const PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";
const ADAPTER: &str = "claude-agent-acp";
const MIN_NODE_MAJOR: u64 = 22;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    #[serde(default = "super::default_true")]
    pub enabled: bool,
    /// An absolute path to the `claude-agent-acp` adapter, otherwise
    /// `claude-agent-acp` on PATH, else `npx`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// The package `npx` runs, e.g. to pin a version; default the latest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_package: Option<String>,
    /// Release verified idle native children; background reads never extend it.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_idle_timeout_secs() -> u64 {
    crate::agents::adapters::acp::session::DEFAULT_IDLE_TIMEOUT_SECS
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            acp_package: None,
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

/// How the adapter is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Runner {
    /// The installed adapter executable.
    Adapter(PathBuf),
    /// `npx -y <package>`; downloads on first use.
    Npx(PathBuf),
}

/// `claude-agent-acp` on PATH, else `npx`. Lookups never execute either:
/// `npx -y` downloads when it runs.
pub(crate) fn choose_runner(lookup: impl Fn(&str) -> Option<PathBuf>) -> Option<Runner> {
    lookup(ADAPTER)
        .map(Runner::Adapter)
        .or_else(|| lookup("npx").map(Runner::Npx))
}

/// The runner and `claude` once every prerequisite is present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prerequisites {
    pub runner: Runner,
    pub claude: PathBuf,
}

/// The first prerequisite that is missing; discovery reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Missing {
    Disabled,
    InvalidBinary,
    InvalidPackage,
    Claude,
    Node,
    NodeTooOld(String),
    Runner,
}

impl Missing {
    pub(crate) fn reason(&self) -> String {
        match self {
            Self::Disabled => "Claude Code native sessions are disabled".into(),
            Self::InvalidBinary => {
                "claude.binary must name an absolute, executable claude-agent-acp".into()
            }
            Self::InvalidPackage => "claude.acp_package is not a package name".into(),
            Self::Claude => "Claude Code (claude) was not found on PATH".into(),
            Self::Node => format!(
                "Node.js {MIN_NODE_MAJOR} or newer is required for Claude Code's ACP adapter; node was not found on PATH"
            ),
            Self::NodeTooOld(found) => format!(
                "Node.js {MIN_NODE_MAJOR} or newer is required for Claude Code's ACP adapter; found {found}"
            ),
            Self::Runner => "Neither claude-agent-acp nor npx was found on PATH".into(),
        }
    }

    fn status(&self) -> AgentAvailability {
        match self {
            Self::Disabled => AgentAvailability::Disabled,
            Self::InvalidBinary | Self::InvalidPackage => AgentAvailability::Unconfigured,
            Self::NodeTooOld(_) => AgentAvailability::Unsupported,
            Self::Claude | Self::Node | Self::Runner => AgentAvailability::NotInstalled,
        }
    }
}

fn node_major(version: &str) -> Option<u64> {
    version
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn valid_package(package: &str) -> bool {
    !package.is_empty()
        && package.len() <= 512
        && !package.starts_with('-')
        && !package.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl ClaudeConfig {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Default::default()
        }
    }

    /// Check every prerequisite. `node_version` is `node --version`'s answer
    /// for the `node` that `lookup` found (`None` if it gave none).
    pub(crate) fn prerequisites(
        &self,
        lookup: impl Fn(&str) -> Option<PathBuf>,
        node_version: Option<&str>,
    ) -> Result<Prerequisites, Missing> {
        if !self.enabled {
            return Err(Missing::Disabled);
        }
        if self
            .acp_package
            .as_deref()
            .is_some_and(|p| !valid_package(p))
        {
            return Err(Missing::InvalidPackage);
        }
        let runner = match &self.binary {
            Some(binary) => {
                let path = PathBuf::from(binary);
                if !path.is_absolute() || !super::is_executable(&path) {
                    return Err(Missing::InvalidBinary);
                }
                Runner::Adapter(path)
            }
            None => choose_runner(&lookup).ok_or(Missing::Runner)?,
        };
        let claude = lookup("claude").ok_or(Missing::Claude)?;
        lookup("node").ok_or(Missing::Node)?;
        match node_version {
            Some(version) if node_major(version).is_some_and(|major| major >= MIN_NODE_MAJOR) => {}
            Some(version) => return Err(Missing::NodeTooOld(version.trim().to_owned())),
            None => return Err(Missing::Node),
        }
        Ok(Prerequisites { runner, claude })
    }

    async fn probe(&self) -> Result<Prerequisites, Missing> {
        let node = crate::agents::tasks::find_on_path("node");
        let version = match &node {
            Some(node) => super::binary_version(node).await,
            None => None,
        };
        self.prerequisites(crate::agents::tasks::find_on_path, version.as_deref())
    }

    /// The program and launch for the adapter; never written anywhere.
    pub(crate) fn launch(&self, found: &Prerequisites) -> (PathBuf, AcpLaunch) {
        let (program, args) = match &found.runner {
            Runner::Adapter(path) => (path.clone(), Vec::new()),
            // The SDK's bundled Claude Code binary is not needed: the adapter
            // runs the user's own `claude` (CLAUDE_CODE_EXECUTABLE below).
            Runner::Npx(path) => (
                path.clone(),
                vec![
                    "-y".to_owned(),
                    "--omit=optional".to_owned(),
                    self.acp_package.clone().unwrap_or_else(|| PACKAGE.into()),
                ],
            ),
        };
        let env = vec![
            ("CLAUDE_AGENT_ACP_PROTOCOL".to_owned(), "v1".to_owned()),
            (
                "CLAUDE_CODE_EXECUTABLE".to_owned(),
                found.claude.to_string_lossy().into_owned(),
            ),
        ];
        (program, AcpLaunch { args, env })
    }
}

pub(super) struct ClaudeState {
    pub(super) config: ClaudeConfig,
    retry_at: tokio::sync::Mutex<Option<tokio::time::Instant>>,
    stopped: std::sync::atomic::AtomicBool,
    start: tokio::sync::Mutex<()>,
    driver: std::sync::OnceLock<std::sync::Arc<crate::agents::adapters::claude::ClaudeDriver>>,
}

impl ClaudeState {
    pub(super) fn new(config: ClaudeConfig) -> Self {
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
        if runtime.manager_for_agent("claude").await.is_some() {
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
        let Ok(found) = self.config.probe().await else {
            return false;
        };
        let Some(version) = claude_version(&found.claude).await else {
            return false;
        };
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let (program, launch) = self.config.launch(&found);
        let directory = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let (manager, driver) = crate::agents::manager::AgentManager::connect_claude(
            program.to_string_lossy().into_owned(),
            directory,
            Some(version),
            runtime.events_tx.clone(),
        );
        driver.set_launch(launch);
        driver.set_idle_timeout(std::time::Duration::from_secs(
            self.config.idle_timeout_secs.max(1),
        ));
        if let Some(servers) = runtime.claude_mcp_servers().await {
            driver.set_mcp_servers(servers);
        }
        let _ = self.driver.set(driver);
        runtime.install("claude", manager, AgentOrigin::None).await;
        true
    }

    pub(super) async fn discovery(&self, runtime: &AgentRuntime) -> AgentDiscoveryInfo {
        let mut info = AgentDiscoveryInfo {
            id: "claude".into(),
            name: "Claude Code".into(),
            kind: "claude".into(),
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
        if let Some(manager) = runtime.manager_for_agent("claude").await {
            // Reachable: the adapter can be started on demand. Neither a live
            // process nor a signed-in Claude Code is implied.
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
        match self.config.probe().await {
            Ok(found) => {
                info.version = claude_version(&found.claude).await;
                info.status = if info.version.is_some() {
                    AgentAvailability::Reachable
                } else {
                    AgentAvailability::Offline
                };
                info.reason = Some(
                    "ACP capabilities and sign-in are checked when opening a Claude Code session"
                        .into(),
                );
            }
            Err(missing) => {
                info.status = missing.status();
                info.reason = Some(missing.reason());
            }
        }
        info
    }
}

impl AgentRuntime {
    pub(crate) async fn shutdown_claude(&self) {
        self.claude
            .stopped
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _start = self.claude.start.lock().await;
        if let Some(manager) = self.manager_for_agent("claude").await {
            manager.shutdown();
        }
        if let Some(driver) = self.claude.driver.get() {
            driver.shutdown().await;
        }
    }

    /// The muqun server as an ACP `mcpServers` entry with a token bound to
    /// Claude Code's native sessions, or `None` when MCP is not wired or the
    /// token could not be minted. Earlier such tokens belonged to children of
    /// a previous Gateway run and are revoked.
    async fn claude_mcp_servers(&self) -> Option<serde_json::Value> {
        let wiring = self.mcp.get()?;
        match super::issue_agent_env_for(
            wiring.tokens.as_ref(),
            &wiring.url,
            "claude",
            "claude (native sessions)",
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
                tracing::warn!("could not mint Claude Code's MCP token: {error}");
                None
            }
        }
    }
}

/// `claude --version` without its " (Claude Code)" suffix; diagnostic only.
async fn claude_version(claude: &std::path::Path) -> Option<String> {
    let version = super::binary_version(claude).await?;
    Some(
        version
            .split_whitespace()
            .next()
            .unwrap_or(&version)
            .to_owned(),
    )
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

    fn lookup(found: &'static [&'static str]) -> impl Fn(&str) -> Option<PathBuf> {
        move |name| {
            found
                .contains(&name)
                .then(|| PathBuf::from(format!("/synthetic/bin/{name}")))
        }
    }

    #[test]
    fn runner_prefers_the_installed_adapter_then_npx() {
        let bin = |name: &str| PathBuf::from(format!("/synthetic/bin/{name}"));
        assert_eq!(
            choose_runner(lookup(&["claude-agent-acp", "npx"])),
            Some(Runner::Adapter(bin("claude-agent-acp")))
        );
        assert_eq!(
            choose_runner(lookup(&["npx", "bunx"])),
            Some(Runner::Npx(bin("npx")))
        );
        assert_eq!(choose_runner(lookup(&["bunx"])), None);
        let config = ClaudeConfig {
            acp_package: Some(format!("{PACKAGE}@0.99.1")),
            ..Default::default()
        };
        let found = config
            .prerequisites(lookup(&["npx", "claude", "node"]), Some("v22.1.0"))
            .unwrap();
        let (program, launch) = config.launch(&found);
        assert_eq!(program, bin("npx"));
        assert_eq!(
            launch.args,
            [
                "-y",
                "--omit=optional",
                "@agentclientprotocol/claude-agent-acp@0.99.1"
            ]
        );
        assert!(launch
            .env
            .contains(&("CLAUDE_AGENT_ACP_PROTOCOL".into(), "v1".into())));
        assert!(launch.env.contains(&(
            "CLAUDE_CODE_EXECUTABLE".into(),
            "/synthetic/bin/claude".into()
        )));
    }

    #[test]
    fn each_missing_prerequisite_is_reported_not_hidden() {
        let ok: &[&str] = &["npx", "claude", "node"];
        for (config, found, node, missing) in [
            (
                ClaudeConfig::default(),
                &["npx", "node"][..],
                Some("v24.0.0"),
                Missing::Claude,
            ),
            (
                ClaudeConfig::default(),
                &["npx", "claude"][..],
                None,
                Missing::Node,
            ),
            (
                ClaudeConfig::default(),
                ok,
                Some("v20.11.1"),
                Missing::NodeTooOld("v20.11.1".into()),
            ),
            (
                ClaudeConfig::default(),
                &["claude", "node"][..],
                Some("v24.0.0"),
                Missing::Runner,
            ),
            (
                ClaudeConfig::disabled(),
                ok,
                Some("v24.0.0"),
                Missing::Disabled,
            ),
            (
                ClaudeConfig {
                    acp_package: Some("--registry=x".into()),
                    ..Default::default()
                },
                ok,
                Some("v24.0.0"),
                Missing::InvalidPackage,
            ),
            (
                ClaudeConfig {
                    binary: Some("claude-agent-acp".into()),
                    ..Default::default()
                },
                ok,
                Some("v24.0.0"),
                Missing::InvalidBinary,
            ),
        ] {
            let found: Vec<&str> = found.to_vec();
            let lookup = |name: &str| {
                found
                    .contains(&name)
                    .then(|| PathBuf::from(format!("/synthetic/bin/{name}")))
            };
            assert_eq!(config.prerequisites(lookup, node), Err(missing.clone()));
        }
        assert_eq!(
            Missing::NodeTooOld("v20.0.0".into()).status(),
            AgentAvailability::Unsupported
        );
        assert_eq!(Missing::Claude.status(), AgentAvailability::NotInstalled);
        assert!(Missing::Claude.reason().contains("claude"));
    }

    #[test]
    fn config_defaults_are_omitted_and_secrets_refused() {
        let config: ClaudeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, ClaudeConfig::default());
        assert!(serde_json::from_str::<ClaudeConfig>(r#"{"token":"secret"}"#).is_err());
    }
}
