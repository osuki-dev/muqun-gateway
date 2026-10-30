//! The Pi side of the runtime: its `config.json` block, and how it is
//! attached, supervised and reported in discovery.
//!
//! Pi is on by default but costs nothing until used: attaching only checks
//! that a `pi` binary answers `--version` and writes the bundled extension
//! to disk. Children are started per session on the first write and closed
//! when idle, so an installed but unused Pi runs no process at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{binary_version, catalog_entry, issue_agent_env_for, settle_agent_tokens};
use super::{AgentOrigin, AgentRuntime};
use crate::agents::adapters::pi::{self, mapper, PiCommandLine, PiDriver, SessionStore};
use crate::agents::manager::AgentManager;

const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;

fn default_true() -> bool {
    true
}

fn default_approvals() -> String {
    "ask".into()
}

fn default_idle_timeout_secs() -> u64 {
    DEFAULT_IDLE_TIMEOUT_SECS
}

/// `pi` in `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PiConfig {
    /// Attach when a `pi` binary is found. On by default: nothing runs until
    /// a session is used.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `pi` on PATH unless set; a value with a separator is a file path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Where Pi keeps session files, if not `~/.pi/agent/sessions`. The
    /// gateway only reads here; it must match where Pi writes (Pi's own
    /// `sessionDir` setting or `PI_CODING_AGENT_SESSION_DIR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    /// `ask`: `bash`, `edit` and `write` wait for an approval in the App.
    /// `never`: Pi runs them as it does on its own.
    #[serde(default = "default_approvals")]
    pub approvals: String,
    /// A child with no run and no command for this long is closed.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

impl Default for PiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            session_dir: None,
            approvals: default_approvals(),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
        }
    }
}

impl PiConfig {
    /// A configuration that never attaches, for runtimes built without one.
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Anything but `never` asks: an unknown value must not silently turn
    /// the approval gate off.
    pub fn asks_approval(&self) -> bool {
        self.approvals.trim() != "never"
    }

    /// The binary to run, or `None` when there is none to find.
    fn binary(&self) -> Option<PathBuf> {
        let name = self
            .binary
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .unwrap_or("pi");
        if name.contains('/') || name.contains(std::path::MAIN_SEPARATOR) {
            let path = PathBuf::from(name);
            return path.is_file().then_some(path);
        }
        crate::agents::tasks::find_on_path(name)
    }

    /// The session root: the configured one, else Pi's own environment
    /// override, else Pi's default.
    fn session_root(&self) -> Option<PathBuf> {
        let configured = self
            .session_dir
            .clone()
            .or_else(|| std::env::var("PI_CODING_AGENT_SESSION_DIR").ok())
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
        match configured {
            Some(dir) => Some(match dir.strip_prefix("~/") {
                Some(rest) => dirs::home_dir()?.join(rest),
                None => PathBuf::from(dir),
            }),
            None => SessionStore::default_root(),
        }
    }
}

/// What the runtime keeps for Pi between supervision passes.
pub(super) struct PiState {
    config: PiConfig,
    /// Where the bundled extension is written. Without one Pi is not
    /// attached: Pi runs that file with the user's rights, so it must sit in
    /// a directory only the user can write, never a shared temp path.
    state_dir: Option<PathBuf>,
    /// Supervision retries every pass; the missing state dir is said once.
    no_state_dir_logged: std::sync::atomic::AtomicBool,
}

impl PiState {
    pub(super) fn new(config: PiConfig, state_dir: Option<PathBuf>) -> Self {
        Self {
            config,
            state_dir,
            no_state_dir_logged: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub(super) fn features(&self) -> crate::discovery::AgentFeatures {
        pi_features(self.config.asks_approval())
    }

    /// Write the bundled extension where `pi -e` can load it. Rewritten on
    /// every attach so an upgraded gateway never runs a stale copy.
    ///
    /// The directory is locked down and the file replaced by an atomic
    /// rename. A symlink at either path is never followed: one planted at the
    /// file is removed first, and a symlinked directory is refused, because
    /// whatever ends up at that path is executed by Pi.
    async fn write_extension(&self) -> anyhow::Result<PathBuf> {
        let state_dir = self
            .state_dir
            .clone()
            .ok_or_else(|| anyhow::anyhow!(NO_STATE_DIR))?;
        tokio::task::spawn_blocking(move || write_extension_into(&state_dir.join("pi")))
            .await
            .map_err(|error| anyhow::anyhow!("extension writer panicked: {error}"))?
    }
}

/// Why Pi is not attached when the gateway runs without a state directory.
const NO_STATE_DIR: &str = "no state directory for the Pi extension";

fn write_extension_into(dir: &Path) -> anyhow::Result<PathBuf> {
    use crate::platform::store::{ensure_secret_dir, write_secret_file};
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        anyhow::bail!(
            "{} is a symlink; refusing to write the Pi extension",
            dir.display()
        );
    }
    ensure_secret_dir(dir)?;
    let path = dir.join("muqun-extension.js");
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&path)?;
    }
    write_secret_file(&path, mapper::EXTENSION_SOURCE.as_bytes())?;
    Ok(path)
}

/// The feature flags the App reads for the `pi` agent.
pub(super) fn pi_features(approvals: bool) -> crate::discovery::AgentFeatures {
    crate::discovery::AgentFeatures {
        streaming: true,
        reasoning_effort: true,
        model_selection: true,
        tool_approvals: approvals,
        worktrees: false,
        revert: false,
        staged_revert: false,
        inbox: false,
        modes: false,
        skills: false,
        slash_commands: false,
        compaction: true,
        background_shells: false,
        // Attachments arrive as host paths in the prompt text.
        attachments: true,
        attachments_by_path: true,
        extra: std::collections::BTreeMap::from([
            ("sessionPagination".into(), serde_json::json!(false)),
            ("timelineEarlier".into(), serde_json::json!(false)),
            ("sessionRename".into(), serde_json::json!(true)),
            ("sessionDelete".into(), serde_json::json!(true)),
            ("sessionFork".into(), serde_json::json!(false)),
        ]),
    }
}

impl AgentRuntime {
    /// One supervision pass for Pi. Returns whether it is attached and well.
    pub(super) async fn supervise_pi(&self) -> bool {
        if !self.pi.enabled() {
            return false;
        }
        if self.keep_if_healthy(pi::KIND).await {
            return true;
        }
        if self.pi.state_dir.is_none() {
            if !self
                .pi
                .no_state_dir_logged
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                tracing::warn!("Pi is not attached: {NO_STATE_DIR}");
            }
            return false;
        }
        let Some(binary) = self.pi.config.binary() else {
            return false;
        };
        let Some(version) = binary_version(&binary).await else {
            tracing::debug!(binary = %binary.display(), "pi does not answer --version");
            return false;
        };
        let Some(root) = self.pi.config.session_root() else {
            tracing::warn!("no home directory to find Pi's sessions in; set pi.session_dir");
            return false;
        };
        let extension = match self.pi.write_extension().await {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!("could not write the Muqun extension for Pi: {error:#}");
                return false;
            }
        };
        // Children reach `/mcp` with their own runtime token, minted per
        // attach; without MCP wiring they start without it.
        let minted = match self.mcp.get() {
            Some(wiring) => {
                match issue_agent_env_for(wiring.tokens.as_ref(), &wiring.url, pi::KIND, "pi (rpc)")
                    .await
                {
                    Ok(minted) => Some(minted),
                    Err(error) => {
                        tracing::warn!("could not mint Pi's MCP token: {error}");
                        None
                    }
                }
            }
            None => None,
        };
        let env = minted
            .as_ref()
            .map(|(env, _)| {
                env.vars()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let driver = PiDriver::new(
            PiCommandLine {
                binary: binary.clone(),
                extension,
            },
            SessionStore::new(root),
            self.pi.config.asks_approval(),
            Duration::from_secs(self.pi.config.idle_timeout_secs.max(1)),
            env,
        );
        let manager = AgentManager::connect_pi(
            Arc::new(driver),
            Some(version.clone()),
            self.events_tx.clone(),
        );
        self.install(pi::KIND, manager, AgentOrigin::Spawned).await;
        if let (Some(wiring), Some((env, previous))) = (self.mcp.get(), &minted) {
            settle_agent_tokens(wiring.tokens.as_ref(), env, previous, true).await;
        }
        tracing::info!(binary = %binary.display(), %version, "attached the Pi agent");
        true
    }

    /// The `pi` entry of `planes.agents.agents[]`.
    pub(super) async fn pi_discovery(&self) -> crate::discovery::AgentDiscoveryInfo {
        use crate::discovery::{AgentAvailability, AgentDiscoveryInfo};
        let mut info = AgentDiscoveryInfo {
            id: pi::KIND.to_string(),
            name: "Pi".to_string(),
            kind: pi::KIND.to_string(),
            status: AgentAvailability::Disabled,
            enabled: self.pi.enabled(),
            endpoint: None,
            version: None,
            reason: None,
            models: Vec::new(),
            modes: Vec::new(),
            features: self.pi.features(),
        };
        if !self.pi.enabled() {
            return info;
        }
        info.endpoint = Some(pi::ENDPOINT.to_string());
        if let Some(manager) = self.manager_for_agent(pi::KIND).await {
            info.status = AgentAvailability::Connected;
            info.version = manager.version();
            (info.models, info.modes) = catalog_entry(manager.agent().as_ref()).await;
            return info;
        }
        // Not attached yet: an installed binary is attached on the next
        // supervision pass, so it is `reachable`.
        match self.pi.config.binary() {
            None => info.status = AgentAvailability::NotInstalled,
            // Never attached: see `PiState::state_dir`.
            Some(_) if self.pi.state_dir.is_none() => {
                info.status = AgentAvailability::Offline;
                info.reason = Some(NO_STATE_DIR.to_string());
            }
            Some(binary) => match binary_version(&binary).await {
                Some(version) => {
                    info.status = AgentAvailability::Reachable;
                    info.version = Some(version);
                }
                None => {
                    info.status = AgentAvailability::Offline;
                    info.reason = Some(format!("{} does not answer --version", path_name(&binary)));
                }
            },
        }
        info
    }
}

fn path_name(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::pi::rpc::{PiChild, PiLaunch};

    /// The config block (defaults, round trip, the approval switch) and the
    /// statuses that need no binary: disabled, then not installed.
    #[tokio::test]
    async fn the_pi_block_round_trips_and_maps_to_a_status() {
        use crate::discovery::AgentAvailability::{Disabled, NotInstalled};
        let missing = Some("/workspace/demo/no-such-pi".to_string());
        let cases = [
            (
                "{}",
                PiConfig {
                    binary: missing.clone(),
                    ..PiConfig::default()
                },
                true,
                NotInstalled,
            ),
            (
                r#"{"enabled":false,"approvals":"never","idle_timeout_secs":60,
                    "session_dir":"/workspace/pi-sessions"}"#,
                PiConfig {
                    enabled: false,
                    binary: None,
                    session_dir: Some("/workspace/pi-sessions".into()),
                    approvals: "never".into(),
                    idle_timeout_secs: 60,
                },
                false,
                Disabled,
            ),
            // An unknown value keeps the gate on.
            (
                r#"{"approvals":"sometimes"}"#,
                PiConfig {
                    approvals: "sometimes".into(),
                    binary: missing.clone(),
                    ..PiConfig::default()
                },
                true,
                NotInstalled,
            ),
        ];
        for (json, want, asks, status) in cases {
            let mut got: PiConfig = serde_json::from_str(json).unwrap();
            // The binary is pointed at nothing so the host's PATH is irrelevant.
            if got.binary.is_none() && got.enabled {
                got.binary = missing.clone();
            }
            assert_eq!(got, want, "{json}");
            assert_eq!(got.asks_approval(), asks, "{json}");
            let back: PiConfig =
                serde_json::from_value(serde_json::to_value(&got).unwrap()).unwrap();
            assert_eq!(back, got, "{json}");

            let runtime = AgentRuntime::with_pi_configs(
                crate::agents::OpencodeConfig {
                    enabled: false,
                    autostart: false,
                    binary: None,
                },
                crate::agents::DeepseekConfig::default(),
                crate::agents::T3Config::default(),
                crate::agents::runtime::CodexConfig {
                    enabled: false,
                    binary: None,
                },
                crate::agents::runtime::FxConfig {
                    enabled: false,
                    binary: None,
                },
                got,
                None,
            );
            assert!(!runtime.supervise_pi().await, "{json}");
            let info = runtime.pi_discovery().await;
            assert_eq!(info.status, status, "{json}");
            assert_eq!(info.features.tool_approvals, asks, "{json}");
            assert_eq!(
                info.endpoint.as_deref(),
                (status != Disabled).then_some(pi::ENDPOINT)
            );
        }
        let default = PiConfig::default();
        assert!(default.enabled && default.asks_approval());
        assert_eq!(default.idle_timeout_secs, 900);
        assert_eq!(
            serde_json::to_string(&PiConfig::default()).unwrap(),
            r#"{"enabled":true,"approvals":"ask","idle_timeout_secs":900}"#
        );
    }

    /// The installed Pi answers `get_state` over RPC. Ignored: it needs the
    /// real binary; it never calls a model.
    #[tokio::test]
    #[ignore]
    async fn pi_real_binary_get_state() {
        let binary = PiConfig::default().binary().expect("a `pi` binary on PATH");
        let cmd = PiCommandLine {
            binary,
            extension: PathBuf::new(),
        };
        let launch = PiLaunch {
            session_file: None,
            cwd: std::env::temp_dir(),
            env: Vec::new(),
            no_extensions: true,
        };
        let child = PiChild::spawn(&cmd, &launch).await.expect("pi starts");
        let state = child
            .request(serde_json::json!({"type": "get_state"}))
            .await
            .expect("get_state succeeds");
        assert!(state.get("sessionId").and_then(|v| v.as_str()).is_some());
        assert_eq!(state.get("isStreaming"), Some(&serde_json::json!(false)));
        child.close().await;
        assert!(!child.is_alive());
    }
}
