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
    /// The terminal sessions (tmux/Herdr) whose panes may hold a Claude Code.
    pub(super) terminals: std::sync::OnceLock<Vec<crate::SessionConfig>>,
}

impl ClaudeState {
    pub(super) fn new(config: ClaudeConfig) -> Self {
        Self {
            config,
            retry_at: tokio::sync::Mutex::new(None),
            stopped: std::sync::atomic::AtomicBool::new(false),
            start: tokio::sync::Mutex::new(()),
            driver: std::sync::OnceLock::new(),
            terminals: std::sync::OnceLock::new(),
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
        match crate::platform::store::state_dir() {
            Ok(dir) => driver.set_owned_store(dir.join("claude-owned-sessions.json")),
            Err(error) => {
                tracing::warn!(%error, "Claude Code session ownership lasts for this run")
            }
        }
        driver.set_terminal_bridge(std::sync::Arc::new(Terminals {
            sessions: self.terminals.get().cloned().unwrap_or_default(),
        }));
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
                    info.modes = catalog
                        .modes
                        .iter()
                        .map(crate::discovery::mode_info_to_agent_mode)
                        .collect();
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
                info.modes = crate::agents::adapters::claude::driver::permission_modes()
                    .iter()
                    .map(crate::discovery::mode_info_to_agent_mode)
                    .collect();
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

/// Claude Code running in a terminal, found through its own session registry
/// (`<config>/sessions/<pid>.json`, see `native_sessions::live_sessions`),
/// and the tmux/Herdr pane whose shell is that process's ancestor.
struct Terminals {
    sessions: Vec<crate::SessionConfig>,
}

/// A pane shell pid that is an ancestor of `pid`, when exactly one is.
pub(crate) fn pane_for(
    pid: u32,
    panes: &[(String, String, u32)],
    descends: impl Fn(u32, u32) -> bool,
) -> Option<(String, String)> {
    let mut found = panes
        .iter()
        .filter(|(_, _, shell)| descends(pid, *shell))
        .map(|(session, pane, _)| (session.clone(), pane.clone()));
    match (found.next(), found.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// The runtime a terminal-held session is shown with.
pub(crate) fn terminal_runtime(
    process: &crate::agents::adapters::claude::native_sessions::LiveProcess,
    pane: Option<(String, String)>,
) -> crate::agents::adapters::acp::session::TerminalLive {
    use crate::agents::adapters::acp::session::{TerminalLive, TerminalRoute};
    use crate::agents::domain::*;
    let status = match process.busy {
        Some(true) => TerminalRuntimeStatus::Working,
        Some(false) => TerminalRuntimeStatus::Idle,
        None => TerminalRuntimeStatus::Unknown,
    };
    match pane {
        Some((session, pane)) => TerminalLive {
            runtime: SessionRuntime {
                transcript: Some(TranscriptSource::Persisted),
                source: RuntimeSource::Terminal,
                interaction: RuntimeInteraction::Terminal,
                status: Some(status),
                terminal: Some(RuntimeTerminal {
                    session_id: session.clone(),
                    pane_id: pane.clone(),
                    instance_id: format!("claude:{}:{}", process.pid, process.start),
                }),
                reason: None,
            },
            route: Some(TerminalRoute { session, pane }),
        },
        // Running outside tmux/Herdr: shown, never typed into, never doubled.
        None => TerminalLive {
            runtime: SessionRuntime {
                transcript: Some(TranscriptSource::Persisted),
                source: RuntimeSource::Unobserved,
                interaction: RuntimeInteraction::Unavailable,
                status: Some(status),
                terminal: None,
                reason: Some(RuntimeReason::TerminalUnavailable),
            },
            route: None,
        },
    }
}

impl Terminals {
    fn session(
        &self,
        id: &str,
    ) -> Result<&crate::SessionConfig, crate::agents::ports::agent::AgentError> {
        self.sessions.iter().find(|s| s.id == id).ok_or_else(|| {
            crate::agents::ports::agent::AgentError::NotAvailable(
                "The terminal holding this conversation is no longer configured".into(),
            )
        })
    }
}

impl crate::agents::adapters::acp::session::TerminalBridge for Terminals {
    fn live<'a>(
        &'a self,
        ids: &'a [String],
    ) -> crate::agents::ports::agent::AgentFuture<
        'a,
        std::collections::HashMap<String, crate::agents::adapters::acp::session::TerminalLive>,
    > {
        Box::pin(async move {
            let registry = tokio::task::spawn_blocking(
                crate::agents::adapters::claude::native_sessions::live_sessions,
            )
            .await
            .unwrap_or_default();
            let wanted: Vec<_> = ids
                .iter()
                .filter_map(|id| {
                    registry
                        .get(id)
                        .map(|process| (id.clone(), process.clone()))
                })
                .collect();
            if wanted.is_empty() {
                return Ok(Default::default());
            }
            let mut panes = Vec::new();
            for session in &self.sessions {
                let listed = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    crate::terminal_backend(session).list_panes(),
                )
                .await;
                if let Ok(Ok(listed)) = listed {
                    panes.extend(listed.into_iter().filter_map(|pane| {
                        Some((
                            session.id.clone(),
                            pane.id.as_str().to_owned(),
                            pane.shell_pid?,
                        ))
                    }));
                }
            }
            let panes = std::sync::Arc::new(panes);
            let mut live = std::collections::HashMap::new();
            for (id, process) in wanted {
                let panes = panes.clone();
                let pid = process.pid;
                let pane = tokio::task::spawn_blocking(move || {
                    pane_for(pid, &panes, crate::terminal::backend::descends_from)
                })
                .await
                .ok()
                .flatten();
                live.insert(id, terminal_runtime(&process, pane));
            }
            Ok(live)
        })
    }

    fn send<'a>(
        &'a self,
        route: &'a crate::agents::adapters::acp::session::TerminalRoute,
        text: &'a str,
    ) -> crate::agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self.session(&route.session)?;
            crate::agents::session_routes::submit_agent_prompt(session, &route.pane, text)
                .await
                .map_err(|error| {
                    tracing::warn!(?error, "typing into the Claude Code terminal failed");
                    crate::agents::ports::agent::AgentError::NotAvailable(
                        "Could not type into the terminal holding this conversation".into(),
                    )
                })?;
            crate::agents::session_routes::schedule_submit_keypress(
                session.clone(),
                route.pane.clone(),
            );
            tracing::info!(pane = %route.pane, "Claude Code prompt typed into its terminal");
            Ok(())
        })
    }

    fn interrupt<'a>(
        &'a self,
        route: &'a crate::agents::adapters::acp::session::TerminalRoute,
    ) -> crate::agents::ports::agent::AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self.session(&route.session)?;
            crate::terminal_backend(session)
                .send_keys(
                    &crate::terminal::backend::PaneId::new(route.pane.clone()),
                    &["Escape".to_owned()],
                )
                .await
                .map_err(|error| {
                    crate::agents::ports::agent::AgentError::NotAvailable(error.to_string())
                })
        })
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
    let mut found = super::acp_features();
    // Verified with adapter 0.99.1 (`docs/claude-acp-protocol.md`); fork and
    // delete follow the live `initialize` once a session has been opened.
    found.modes = true;
    for key in [
        "modeSwitching",
        "sessionRename",
        "sessionDelete",
        "sessionFork",
        "promptSteering",
    ] {
        found.extra.insert(key.into(), serde_json::json!(true));
    }
    // Prompts and Stop for a conversation open in a terminal pane are typed
    // into that pane (`TerminalBridge`); Apps that know this may send them.
    found
        .extra
        .insert("terminalInput".into(), serde_json::json!(true));
    found
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
    fn a_terminal_process_maps_to_exactly_one_pane_or_is_read_only() {
        use crate::agents::adapters::claude::native_sessions::LiveProcess;
        use crate::agents::domain::{RuntimeInteraction, RuntimeReason, TerminalRuntimeStatus};
        // claude 300 runs under shell 30 (pane a); shell 40 holds nothing of it.
        let tree = |pid: u32, ancestor: u32| pid == ancestor || (pid == 300 && ancestor == 30);
        let pane = |session: &str, pane: &str, shell| (session.to_owned(), pane.to_owned(), shell);
        for (panes, expected) in [
            (
                vec![pane("s", "a", 30), pane("s", "b", 40)],
                Some(("s", "a")),
            ),
            (vec![pane("s", "b", 40)], None),
            // Two panes claiming one process: never guess.
            (vec![pane("s", "a", 30), pane("t", "a", 30)], None),
        ] {
            let found = pane_for(300, &panes, tree);
            assert_eq!(found, expected.map(|(s, p)| (s.to_owned(), p.to_owned())));
        }
        let process = LiveProcess {
            pid: 300,
            start: "9".into(),
            busy: Some(true),
        };
        let typed = terminal_runtime(&process, Some(("s".into(), "a".into())));
        assert_eq!(typed.runtime.interaction, RuntimeInteraction::Terminal);
        assert_eq!(typed.runtime.status, Some(TerminalRuntimeStatus::Working));
        assert_eq!(typed.runtime.terminal.unwrap().instance_id, "claude:300:9");
        assert_eq!(typed.route.unwrap().pane, "a");
        let shown = terminal_runtime(&process, None);
        assert_eq!(shown.runtime.interaction, RuntimeInteraction::Unavailable);
        assert_eq!(
            shown.runtime.reason,
            Some(RuntimeReason::TerminalUnavailable)
        );
        assert!(shown.route.is_none());
    }

    #[test]
    fn config_defaults_are_omitted_and_secrets_refused() {
        let config: ClaudeConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, ClaudeConfig::default());
        assert!(serde_json::from_str::<ClaudeConfig>(r#"{"token":"secret"}"#).is_err());
    }
}
