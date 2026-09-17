//! Keeping an OpenCode engine attached for the life of the gateway.
//!
//! Discovery used to run once at startup and the result was final: if OpenCode
//! was not running at that moment every agent route answered 503 for the life
//! of the process, and if OpenCode restarted on a new port -- its port is
//! ephemeral -- the gateway kept the dead URL forever. Restarting OpenCode
//! meant restarting the gateway.
//!
//! This supervises instead. It adopts a healthy service, starts one when there
//! is none and the owner has left autostart on, re-discovers whenever the
//! stream or the health probe says the engine has gone, and hands every route
//! whatever manager is current.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};

use super::adapters::opencode::OpencodeEndpoint;
use super::domain::AgentDomainEvent;
use super::manager::AgentManager;

/// How often the supervisor checks a healthy engine.
const HEALTHY_POLL: Duration = Duration::from_secs(15);
/// How soon it retries after finding none.
const UNHEALTHY_POLL: Duration = Duration::from_secs(3);
/// How long a freshly spawned `opencode serve --service` is given to register.
const STARTUP_WAIT: Duration = Duration::from_secs(20);
const STARTUP_POLL: Duration = Duration::from_millis(400);
/// Backoff ceiling between failed start attempts.
const MAX_START_BACKOFF: Duration = Duration::from_secs(120);

/// `opencode` in `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeConfig {
    /// Start `opencode serve --service` when no healthy service is found.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// The binary to start. Resolved on `PATH` when absent -- the systemd unit
    /// already puts `~/.opencode/bin` first, which is where v2 lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            binary: None,
        }
    }
}

/// How the engine currently attached was obtained, for the status route and
/// the log line an operator reads after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineOrigin {
    /// A service that was already running.
    Adopted,
    /// One this gateway started.
    Spawned,
    /// Nothing attached.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    pub available: bool,
    pub origin: EngineOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub stream_connected: bool,
    pub autostart: bool,
}

pub struct AgentRuntime {
    manager: RwLock<Option<Arc<AgentManager>>>,
    /// The event channel belongs to the runtime, not to a manager, so a
    /// subscriber keeps its stream across a reconnect.
    events_tx: broadcast::Sender<AgentDomainEvent>,
    config: OpencodeConfig,
    origin: RwLock<EngineOrigin>,
    /// The child this gateway started, if any. Held so a second one is never
    /// spawned while the first is alive.
    child: Mutex<Option<tokio::process::Child>>,
    supervising: AtomicBool,
}

impl AgentRuntime {
    pub fn new(config: OpencodeConfig) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            manager: RwLock::new(None),
            events_tx,
            config,
            origin: RwLock::new(EngineOrigin::None),
            child: Mutex::new(None),
            supervising: AtomicBool::new(false),
        })
    }

    /// A runtime that will never attach an engine, for tests and for a build
    /// of `AppState` that has no business starting anything.
    pub fn disabled() -> Arc<Self> {
        Self::new(OpencodeConfig {
            autostart: false,
            binary: None,
        })
    }

    /// The engine as it stands, or `None` while nothing is attached. Every
    /// agent route asks here rather than holding a manager of its own.
    pub async fn manager(&self) -> Option<Arc<AgentManager>> {
        self.manager.read().await.clone()
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events_tx.subscribe()
    }

    pub async fn status(&self) -> EngineStatus {
        let manager = self.manager.read().await.clone();
        EngineStatus {
            available: manager.is_some(),
            origin: *self.origin.read().await,
            url: manager.as_ref().map(|m| m.endpoint_url().to_string()),
            version: manager
                .as_ref()
                .and_then(|m| m.driver().client().endpoint.version.clone()),
            stream_connected: manager.as_ref().map(|m| m.stream_connected()).unwrap_or(false),
            autostart: self.config.autostart,
        }
    }

    /// Attach an engine now, and keep one attached. Safe to call once.
    pub fn spawn_supervisor(self: &Arc<Self>) {
        if self.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut start_backoff = Duration::from_secs(2);
            loop {
                let healthy = runtime.check_and_repair(&mut start_backoff).await;
                tokio::time::sleep(if healthy { HEALTHY_POLL } else { UNHEALTHY_POLL }).await;
            }
        });
    }

    /// One supervision pass. Returns whether an engine is attached and well.
    async fn check_and_repair(&self, start_backoff: &mut Duration) -> bool {
        let current = self.manager.read().await.clone();

        // Whatever the registration file says now wins: OpenCode's port is
        // ephemeral, so a restart moves it and the old URL is dead.
        let discovered = OpencodeEndpoint::discover().await;

        if let Some(manager) = current {
            let same_endpoint = discovered
                .as_ref()
                .map(|e| e.url == manager.endpoint_url())
                .unwrap_or(false);
            if same_endpoint && self.probe(manager.endpoint_url()).await {
                return true;
            }
            tracing::warn!(
                url = manager.endpoint_url(),
                "opencode engine unhealthy or moved, re-discovering"
            );
            manager.shutdown();
            *self.manager.write().await = None;
            *self.origin.write().await = EngineOrigin::None;
        }

        // Adopt anything already healthy before starting anything.
        if let Some(endpoint) = discovered {
            if endpoint.probe_healthy(&probe_client()).await {
                self.attach(endpoint, EngineOrigin::Adopted).await;
                *start_backoff = Duration::from_secs(2);
                return true;
            }
        }

        if !self.config.autostart {
            tracing::debug!("no opencode service and autostart is off");
            return false;
        }

        match self.start_service().await {
            Ok(endpoint) => {
                self.attach(endpoint, EngineOrigin::Spawned).await;
                *start_backoff = Duration::from_secs(2);
                true
            }
            Err(err) => {
                tracing::warn!(%err, backoff_s = start_backoff.as_secs(), "could not start opencode");
                tokio::time::sleep(*start_backoff).await;
                *start_backoff = (*start_backoff * 2).min(MAX_START_BACKOFF);
                false
            }
        }
    }

    async fn attach(&self, endpoint: OpencodeEndpoint, origin: EngineOrigin) {
        let url = endpoint.url.clone();
        let version = endpoint.version.clone();
        let manager = Arc::new(AgentManager::connect(endpoint, self.events_tx.clone()));
        *self.manager.write().await = Some(manager);
        *self.origin.write().await = origin;
        match origin {
            EngineOrigin::Adopted => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                "adopted the running OpenCode service"
            ),
            EngineOrigin::Spawned => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                "started an OpenCode service and attached to it"
            ),
            EngineOrigin::None => {}
        }
    }

    async fn probe(&self, url: &str) -> bool {
        let Some(manager) = self.manager.read().await.clone() else {
            return false;
        };
        let endpoint = manager.driver().client().endpoint.clone();
        if endpoint.url != url {
            return false;
        }
        endpoint.probe_healthy(&probe_client()).await
    }

    /// Start `opencode serve --service`, detached, and wait for it to register.
    ///
    /// Never more than one: a child this gateway started and has not reaped is
    /// given the benefit of the doubt, and a service someone else is running is
    /// adopted before this is ever reached.
    async fn start_service(&self) -> anyhow::Result<OpencodeEndpoint> {
        let mut slot = self.child.lock().await;
        if let Some(ref mut child) = *slot {
            match child.try_wait() {
                // Still running: it simply has not registered yet.
                Ok(None) => {
                    drop(slot);
                    return wait_for_service().await;
                }
                Ok(Some(status)) => {
                    tracing::warn!(%status, "the OpenCode service this gateway started has exited");
                }
                Err(err) => {
                    tracing::warn!(%err, "could not check the spawned OpenCode service");
                }
            }
            *slot = None;
        }

        let binary = self
            .config
            .binary
            .clone()
            .unwrap_or_else(|| "opencode".to_string());
        tracing::info!(binary = %binary, "no OpenCode service found, starting one");

        let mut command = tokio::process::Command::new(&binary);
        command
            .arg("serve")
            .arg("--service")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Its own process group, so it survives this gateway and does not
            // take a terminal signal meant for it.
            .process_group(0)
            .kill_on_drop(false);

        let child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("could not run `{binary} serve --service`: {e}"))?;
        tracing::info!(pid = child.id(), "OpenCode service starting");
        *slot = Some(child);
        drop(slot);

        wait_for_service().await
    }
}

fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
}

/// Poll the registration file until the service answers its health probe, or
/// give up after `STARTUP_WAIT`. Bounded on purpose: the caller backs off and
/// tries again rather than blocking the supervisor forever.
async fn wait_for_service() -> anyhow::Result<OpencodeEndpoint> {
    let client = probe_client();
    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    loop {
        if let Some(endpoint) = OpencodeEndpoint::discover().await {
            if endpoint.probe_healthy(&client).await {
                return Ok(endpoint);
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "the OpenCode service did not become healthy within {}s",
                STARTUP_WAIT.as_secs()
            );
        }
        tokio::time::sleep(STARTUP_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_defaults_to_on_and_the_binary_is_optional() {
        let config: OpencodeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(config.autostart);
        assert!(config.binary.is_none());
    }

    #[test]
    fn autostart_can_be_turned_off() {
        let config: OpencodeConfig =
            serde_json::from_str(r#"{"autostart":false,"binary":"/opt/opencode"}"#)
                .expect("config parses");
        assert!(!config.autostart);
        assert_eq!(config.binary.as_deref(), Some("/opt/opencode"));
    }

    #[tokio::test]
    async fn a_disabled_runtime_attaches_nothing_and_never_spawns() {
        let runtime = AgentRuntime::disabled();
        assert!(runtime.manager().await.is_none());
        let status = runtime.status().await;
        assert!(!status.available);
        assert!(!status.autostart);
        assert_eq!(status.origin, EngineOrigin::None);
        // A subscriber works with no engine attached, which is what lets a
        // stream outlive a reconnect.
        let _rx = runtime.subscribe_events();
    }
}
