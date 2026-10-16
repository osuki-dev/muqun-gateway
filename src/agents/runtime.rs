//! Keeping an OpenCode agent attached for the life of the gateway.
//!
//! Discovery used to run once at startup and the result was final: if OpenCode
//! was not running at that moment every agent route answered 503 for the life
//! of the process, and if OpenCode restarted on a new port -- its port is
//! ephemeral -- the gateway kept the dead URL forever. Restarting OpenCode
//! meant restarting the gateway.
//!
//! This supervises instead. It adopts a healthy service, starts one when there
//! is none and the owner has left autostart on, re-discovers whenever the
//! stream or the health probe says the agent has gone, and hands every route
//! whatever manager is current.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex, RwLock};

use super::adapters::opencode::OpencodeEndpoint;
use super::domain::AgentDomainEvent;
use super::manager::AgentManager;

pub(crate) mod codex;
pub use codex::CodexConfig;
pub(crate) mod deepseek;
mod t3;
use t3::{catalog_entry, T3State};
pub use t3::{T3Config, DEFAULT_T3_URL};

/// How often the supervisor checks a healthy agent.
const HEALTHY_POLL: Duration = Duration::from_secs(15);
/// How soon it retries after finding none.
const UNHEALTHY_POLL: Duration = Duration::from_secs(3);
/// How long service startup and registration may each take.
const STARTUP_WAIT: Duration = Duration::from_secs(20);
const STARTUP_POLL: Duration = Duration::from_millis(400);
/// Backoff ceiling between failed start attempts.
const MAX_START_BACKOFF: Duration = Duration::from_secs(120);
/// How often the supervisor glances at the event stream while the agent is
/// otherwise healthy. A lost stream *is* the agent going away, and waiting
/// for the next health poll to notice it cost thirteen seconds of silence in
/// the app for no reason -- the flag is an atomic read, so this is nearly free.
const STREAM_WATCH_TICK: Duration = Duration::from_secs(1);
/// The wait before re-discovering after the stream dropped. Nothing the first
/// time: look at once. Then doubling, so a stream that flaps cannot spin the
/// supervisor.
const STREAM_LOSS_FIRST_WAIT: Duration = Duration::ZERO;
const STREAM_LOSS_MAX_WAIT: Duration = Duration::from_secs(30);
/// The agent major version this gateway speaks. v1 is a different API, and
/// half-working with it is worse than saying so.
const SUPPORTED_OPENCODE_MAJOR: u64 = 2;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Each version stream is bounded independently, including diagnostic output.
const VERSION_OUTPUT_LIMIT: u64 = 4096;

/// Preference among attached providers. Explicit provider/session selections
/// retain their route; discovery additionally puts usable providers first.
pub const AGENT_PREFERENCE: [&str; 4] = ["codex", "opencode", "deepseek", "t3"];
/// Most session-to-agent routes remembered; the oldest go first.
const MAX_SESSION_ROUTES: usize = 4096;
/// How long an agent discovery result is reused.
const DISCOVERY_TTL: Duration = Duration::from_secs(10);
/// The longest a request may wait on upstream probing.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// The longest one agent may take to say whether it owns a session.
const SESSION_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// The entry of `map` for the most preferred agent present.
fn pick_primary<T>(map: &BTreeMap<String, T>) -> Option<&T> {
    AGENT_PREFERENCE.iter().find_map(|id| map.get(*id))
}

/// Remembered session routes, bounded: insertion order is kept so the oldest
/// can be dropped.
#[derive(Default)]
struct SessionRoutes {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl SessionRoutes {
    fn insert(&mut self, asid: &str, agent_id: &str) {
        if self
            .map
            .insert(asid.to_string(), agent_id.to_string())
            .is_none()
        {
            self.order.push_back(asid.to_string());
        }
        while self.map.len() > MAX_SESSION_ROUTES {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.map.remove(&oldest);
                }
                None => break,
            }
        }
    }

    fn remove(&mut self, asid: &str) {
        if self.map.remove(asid).is_some() {
            self.order.retain(|a| a != asid);
        }
    }
}

/// `opencode` in `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpencodeConfig {
    /// Explicitly enable or disable the OpenCode agent.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Run `opencode service start` when no healthy service is found.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// The binary to start, when `PATH` is not the right answer.
    ///
    /// Absent, the gateway runs whatever `opencode` `PATH` resolves to, and
    /// says which file that turned out to be. It does not go looking in an
    /// install directory of its own: where OpenCode lives differs per OS and
    /// per install, and a gateway guessing at it would quietly run a different
    /// binary than the one the owner's shell does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

fn default_true() -> bool {
    true
}

impl Default for OpencodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            autostart: true,
            binary: None,
        }
    }
}

/// `deepseek` in `config.json`.
///
/// `autostart` uses the same serde default as [`OpencodeConfig::autostart`]
/// (`default_true`). Spawn still requires `enabled`, which defaults to false,
/// so a config written before this key existed does not start a process until
/// DeepSeek is turned on. Once `enabled` is set and `autostart` is omitted,
/// the gateway starts the harness. Set `autostart` to false to only adopt a
/// harness you started yourself.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeepseekConfig {
    /// Explicitly enable the DeepSeek agent.
    #[serde(default)]
    pub enabled: bool,
    /// Start `dsh web` when `enabled` and no healthy endpoint is listening.
    ///
    /// Defaults to true when the key is absent. See the struct comment for
    /// why an old config without the key stays quiet until DeepSeek is enabled.
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Service URL. When set, this host and port are what a spawned harness
    /// listens on, and adoption probes this URL only. When absent, adoption
    /// uses `DEEPSEEK_HARNESS_URL` / `DSH_URL` exclusively when set (attach-only),
    /// otherwise tries port 3080, then the legacy port 19387. Without an
    /// explicit endpoint or environment override, a spawned harness listens on
    /// `http://127.0.0.1:3080`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Optional DSH launch token, exchanged for the service's root-session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Optional signing secret for browser-session cookie
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl Default for DeepseekConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            autostart: true,
            endpoint: None,
            token: None,
            secret: None,
        }
    }
}

/// `Debug` never prints the token or the cookie secret: `Config` derives
/// `Debug` and is logged at startup, and credentials in the journal are a leak.
impl std::fmt::Debug for DeepseekConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |value: &Option<String>| value.as_ref().map(|_| "<redacted>");
        f.debug_struct("DeepseekConfig")
            .field("enabled", &self.enabled)
            .field("autostart", &self.autostart)
            .field("endpoint", &self.endpoint)
            .field("token", &redact(&self.token))
            .field("secret", &redact(&self.secret))
            .finish()
    }
}

/// How the agent currently attached was obtained, for the status route and
/// the log line an operator reads after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOrigin {
    /// A service that was already running.
    Adopted,
    /// One this gateway started.
    Spawned,
    /// Nothing attached.
    None,
}

/// Whether this gateway can establish that OpenCode is installed locally.
///
/// This deliberately says nothing about whether a service is currently
/// reachable. An externally configured endpoint is not necessarily local, so
/// its installation cannot be determined from this process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstallation {
    Installed,
    NotFound,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub available: bool,
    pub installation: AgentInstallation,
    pub origin: AgentOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub stream_connected: bool,
    pub autostart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The agent this status describes; the agent's `kind()`. Absent while
    /// nothing is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

pub struct AgentRuntime {
    manager: RwLock<Option<Arc<AgentManager>>>,
    managers: RwLock<BTreeMap<String, Arc<AgentManager>>>,
    session_routes: RwLock<SessionRoutes>,
    /// The last agent discovery and when it was taken.
    discovery_cache: RwLock<Option<(tokio::time::Instant, crate::discovery::AgentPlaneDiscovery)>>,
    /// One refresh at a time; the rest wait and read its result.
    discovery_refresh: Mutex<()>,
    /// The event channel belongs to the runtime, not to a manager, so a
    /// subscriber keeps its stream across a reconnect.
    events_tx: broadcast::Sender<AgentDomainEvent>,
    config: OpencodeConfig,
    deepseek_config: DeepseekConfig,
    t3: T3State,
    codex: codex::CodexState,
    /// The harness this gateway started, if any. Not a kill handle.
    deepseek_spawn: Mutex<deepseek::SpawnSlot>,
    /// Origin of the attached DeepSeek manager, including when it is not primary.
    deepseek_origin: RwLock<AgentOrigin>,
    origin: RwLock<AgentOrigin>,
    /// Identity last verified for the attached OpenCode manager. `/api/info`
    /// has no instance id, so retain that field from registration separately.
    opencode_instance: RwLock<Option<OpencodeInstance>>,
    /// Serialize startup commands; OpenCode owns the background service.
    start_lock: Mutex<()>,
    supervising: AtomicBool,
    /// Where OpenCode's `MUQUN_MCP_*` environment comes from; set once the
    /// token store exists (the runtime is built before it).
    mcp: std::sync::OnceLock<McpWiring>,
    terminal_runtime: std::sync::OnceLock<Arc<super::terminal_runtime::TerminalRuntime>>,
    native_codex_sessions:
        std::sync::OnceLock<Arc<super::adapters::codex::native_sessions::NativeSessions>>,
}

#[derive(PartialEq, Eq)]
struct OpencodeInstance {
    registration_id: Option<String>,
    url: String,
    version: Option<String>,
    pid: Option<u32>,
    // Credential rotation must reconnect the manager's client and SSE stream.
    password: Option<String>,
}

impl OpencodeInstance {
    fn new(endpoint: &OpencodeEndpoint, registration_id: Option<String>) -> Self {
        Self {
            registration_id,
            url: endpoint.url.clone(),
            version: endpoint.version.clone(),
            pid: endpoint.pid,
            password: endpoint.password.clone(),
        }
    }
}

/// What the gateway needs to mint OpenCode's assistant token.
struct McpWiring {
    tokens: Arc<dyn crate::mcp::tokens::AssistantTokenStore>,
    /// The `/mcp` URL OpenCode is given.
    url: String,
}

/// The environment that lets an agent started by the gateway reach `/mcp`.
pub(crate) struct McpEnv {
    token_id: String,
    token: String,
    url: String,
}

impl McpEnv {
    fn vars(&self) -> [(&'static str, &str); 2] {
        [
            ("MUQUN_MCP_TOKEN", &self.token),
            ("MUQUN_MCP_URL", &self.url),
        ]
    }

    fn apply(&self, command: &mut tokio::process::Command) {
        for (name, value) in self.vars() {
            command.env(name, value);
        }
    }
}

impl AgentRuntime {
    pub fn new(config: OpencodeConfig) -> Arc<Self> {
        Self::with_configs(config, DeepseekConfig::default(), T3Config::default(), None)
    }

    /// `state_dir` is where credentials an agent hands the gateway (the T3
    /// bearer) are kept; `None` keeps them in memory for this run only.
    pub fn with_configs(
        config: OpencodeConfig,
        deepseek_config: DeepseekConfig,
        t3_config: T3Config,
        state_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        Self::with_all_configs(
            config,
            deepseek_config,
            t3_config,
            CodexConfig::default(),
            state_dir,
        )
    }

    pub fn with_all_configs(
        config: OpencodeConfig,
        deepseek_config: DeepseekConfig,
        t3_config: T3Config,
        codex_config: CodexConfig,
        state_dir: Option<std::path::PathBuf>,
    ) -> Arc<Self> {
        Self::with_states(
            config,
            deepseek_config,
            T3State::new(t3_config, state_dir),
            codex::CodexState::new(codex_config),
        )
    }

    fn with_t3_state(
        config: OpencodeConfig,
        deepseek_config: DeepseekConfig,
        t3: T3State,
    ) -> Arc<Self> {
        Self::with_states(
            config,
            deepseek_config,
            t3,
            codex::CodexState::new(CodexConfig::default()),
        )
    }

    fn with_states(
        config: OpencodeConfig,
        deepseek_config: DeepseekConfig,
        t3: T3State,
        codex: codex::CodexState,
    ) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(1024);
        Arc::new(Self {
            manager: RwLock::new(None),
            managers: RwLock::new(BTreeMap::new()),
            session_routes: RwLock::new(SessionRoutes::default()),
            discovery_cache: RwLock::new(None),
            discovery_refresh: Mutex::new(()),
            events_tx,
            config,
            deepseek_config,
            t3,
            codex,
            deepseek_spawn: Mutex::new(deepseek::SpawnSlot::default()),
            deepseek_origin: RwLock::new(AgentOrigin::None),
            origin: RwLock::new(AgentOrigin::None),
            opencode_instance: RwLock::new(None),
            start_lock: Mutex::new(()),
            supervising: AtomicBool::new(false),
            mcp: std::sync::OnceLock::new(),
            terminal_runtime: std::sync::OnceLock::new(),
            native_codex_sessions: std::sync::OnceLock::new(),
        })
    }

    pub(crate) fn observe_terminal_sessions(&self, sessions: &[crate::SessionConfig]) {
        let _ = self
            .terminal_runtime
            .set(super::terminal_runtime::TerminalRuntime::new(sessions));
    }

    pub(crate) fn restore_native_codex_sessions(
        &self,
        db: Arc<crate::platform::store::sqlite::SqliteStore>,
    ) {
        let _ = self
            .native_codex_sessions
            .set(super::adapters::codex::native_sessions::NativeSessions::new(db));
    }

    /// Let the OpenCode service this runtime starts reach `/mcp` at `url`.
    /// Not called when `mcp.enabled` is false.
    pub(crate) fn enable_mcp(
        &self,
        tokens: Arc<dyn crate::mcp::tokens::AssistantTokenStore>,
        url: String,
    ) {
        let _ = self.mcp.set(McpWiring { tokens, url });
    }

    /// A fresh OpenCode agent token and the ids of the earlier ones (revoked
    /// by [`settle_agent_tokens`] once the start is known to have worked), or
    /// `None` when MCP is not wired or minting failed -- OpenCode then starts
    /// as it always did.
    async fn opencode_mcp_env(&self) -> Option<(McpEnv, Vec<String>)> {
        let wiring = self.mcp.get()?;
        match issue_agent_env(wiring.tokens.as_ref(), &wiring.url).await {
            Ok(minted) => Some(minted),
            Err(err) => {
                tracing::warn!("could not mint OpenCode's MCP token: {err}");
                None
            }
        }
    }

    /// A runtime that will never attach an agent, for tests and for a build
    /// of `AppState` that has no business starting anything.
    pub fn disabled() -> Arc<Self> {
        Self::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig {
                enabled: false,
                binary: None,
            },
            None,
        )
    }

    /// The default or primary agent manager, or `None` while nothing is attached.
    pub async fn manager(&self) -> Option<Arc<AgentManager>> {
        self.manager.read().await.clone()
    }

    /// Attach a ready-made manager under its agent id, as `attach` does.
    #[cfg(test)]
    pub(crate) async fn attach_for_test(&self, manager: Arc<AgentManager>) {
        let kind = manager.agent().kind();
        if kind == "deepseek" {
            *self.deepseek_origin.write().await = AgentOrigin::Adopted;
        }
        self.managers
            .write()
            .await
            .insert(kind.to_string(), manager);
        self.promote_primary(kind, AgentOrigin::Adopted).await;
    }

    /// Return all currently active agent managers.
    pub async fn all_managers(&self) -> Vec<Arc<AgentManager>> {
        let map = self.managers.read().await;
        if map.is_empty() {
            if let Some(m) = self.manager.read().await.clone() {
                return vec![m];
            }
            Vec::new()
        } else {
            map.values().cloned().collect()
        }
    }

    /// Return manager for a specific agent id (e.g. "opencode", "deepseek").
    pub async fn manager_for_agent(&self, agent_id: &str) -> Option<Arc<AgentManager>> {
        let map = self.managers.read().await;
        if let Some(m) = map.get(agent_id) {
            return Some(m.clone());
        }
        if let Some(m) = self.manager.read().await.clone() {
            if m.agent().kind() == agent_id {
                return Some(m);
            }
        }
        None
    }

    /// Whether `agent_id` is an id this gateway knows, attached or not.
    pub fn is_known_agent(&self, agent_id: &str) -> bool {
        AGENT_PREFERENCE.contains(&agent_id)
    }

    /// Attached managers with their agent ids, most preferred first.
    async fn attached_in_order(&self) -> Vec<(String, Arc<AgentManager>)> {
        let map = self.managers.read().await;
        let mut out: Vec<_> = AGENT_PREFERENCE
            .iter()
            .filter_map(|id| map.get(*id).map(|m| (id.to_string(), m.clone())))
            .collect();
        if out.is_empty() {
            if let Some(m) = self.manager.read().await.clone() {
                out.push((m.agent().kind().to_string(), m));
            }
        }
        out
    }

    /// The manager that owns a session: the recorded route, else whichever
    /// attached agent says it has the session (and that is then recorded).
    /// `None` when nothing owns it.
    pub async fn manager_for_session(&self, asid: &str) -> Option<Arc<AgentManager>> {
        let recorded = self.session_routes.read().await.map.get(asid).cloned();
        if let Some(agent_id) = recorded {
            if let Some(m) = self.manager_for_agent(&agent_id).await {
                return Some(m);
            }
        }
        // Probed at once, each under its own timeout; the first owner in
        // preference order is chosen, as the serial walk chose it.
        let attached = self.attached_in_order().await;
        let owns = futures::future::join_all(attached.iter().map(|(_, manager)| {
            tokio::time::timeout(SESSION_LOOKUP_TIMEOUT, manager.agent().get_session(asid))
        }))
        .await;
        for ((agent_id, manager), owns) in attached.into_iter().zip(owns) {
            if matches!(owns, Ok(Ok(_))) {
                self.record_session_route(asid, &agent_id).await;
                return Some(manager);
            }
        }
        None
    }

    /// Record routing for a session id to its owning agent.
    pub async fn record_session_route(&self, asid: &str, agent_id: &str) {
        self.session_routes.write().await.insert(asid, agent_id);
    }

    /// Drop the route of a session that no longer exists.
    pub async fn forget_session_route(&self, asid: &str) {
        self.session_routes.write().await.remove(asid);
    }

    /// Forget a deleted session without clearing another agent's same-id route.
    pub(crate) async fn forget_session_route_for_agent(&self, asid: &str, agent_id: &str) {
        let mut routes = self.session_routes.write().await;
        if routes.map.get(asid).is_some_and(|owner| owner == agent_id) {
            routes.remove(asid);
        }
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.events_tx.subscribe()
    }

    /// Put an event on the runtime's channel, as an attached agent would.
    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, event: AgentDomainEvent) {
        let _ = self.events_tx.send(event);
    }

    pub async fn status(&self) -> AgentStatus {
        let manager = self.manager.read().await.clone();
        AgentStatus {
            available: manager
                .as_ref()
                .is_some_and(|m| m.agent().kind() != "codex" || m.stream_connected()),
            installation: if manager.is_some() {
                AgentInstallation::Installed
            } else {
                local_installation_status(&self.config)
            },
            origin: *self.origin.read().await,
            url: manager.as_ref().map(|m| m.endpoint_url().to_string()),
            version: manager.as_ref().and_then(|m| m.version()),
            stream_connected: manager
                .as_ref()
                .map(|m| m.stream_connected())
                .unwrap_or(false),
            autostart: if manager
                .as_ref()
                .is_some_and(|m| m.agent().kind() == "codex")
            {
                self.codex.config.enabled
            } else {
                self.config.autostart
            },
            kind: manager.as_ref().map(|m| m.agent().kind().to_string()),
            agent_id: manager.as_ref().map(|m| m.agent().kind().to_string()),
        }
    }

    /// Status of one attached agent, `None` when it is not attached. The
    /// primary answers exactly as `status()` does. Other service agents are
    /// adopted; a Codex private fallback remains owned by Gateway.
    pub async fn status_for(&self, agent_id: &str) -> Option<AgentStatus> {
        let manager = self.manager_for_agent(agent_id).await?;
        let primary = self.manager.read().await.clone();
        if primary.is_some_and(|p| Arc::ptr_eq(&p, &manager)) {
            return Some(self.status().await);
        }
        let kind = manager.agent().kind().to_string();
        Some(AgentStatus {
            available: kind != "codex" || manager.stream_connected(),
            installation: AgentInstallation::Installed,
            origin: if kind == "deepseek" {
                *self.deepseek_origin.read().await
            } else if kind == "codex"
                && manager.endpoint_url() == "stdio://private-codex-app-server"
            {
                AgentOrigin::Spawned
            } else {
                AgentOrigin::Adopted
            },
            url: Some(manager.endpoint_url().to_string()),
            version: manager.version(),
            stream_connected: manager.stream_connected(),
            autostart: kind == "codex" && self.codex.config.enabled,
            kind: Some(kind.clone()),
            agent_id: Some(kind),
        })
    }

    /// Status and capabilities of all configured or reachable AI agents.
    ///
    /// Served from a short-lived cache so unauthenticated discovery requests do
    /// not each probe upstream. A refresh that overruns `DISCOVERY_TIMEOUT`
    /// yields the stale result, or a snapshot of what is attached if there is
    /// none.
    pub async fn discover_agents(&self) -> crate::discovery::AgentPlaneDiscovery {
        let fresh =
            |cache: &Option<(tokio::time::Instant, crate::discovery::AgentPlaneDiscovery)>| {
                cache
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < DISCOVERY_TTL)
                    .map(|(_, d)| d.clone())
            };
        if let Some(hit) = fresh(&*self.discovery_cache.read().await) {
            return self.fence_codex_discovery(hit).await;
        }
        let refresh = async {
            let _one = self.discovery_refresh.lock().await;
            if let Some(hit) = fresh(&*self.discovery_cache.read().await) {
                return hit;
            }
            let found = self.probe_agents().await;
            *self.discovery_cache.write().await =
                Some((tokio::time::Instant::now(), found.clone()));
            found
        };
        let found = match tokio::time::timeout(DISCOVERY_TIMEOUT, refresh).await {
            Ok(found) => found,
            Err(_) => {
                tracing::warn!("agent discovery timed out");
                let stale = self
                    .discovery_cache
                    .read()
                    .await
                    .as_ref()
                    .map(|(_, d)| d.clone());
                match stale {
                    Some(d) => d,
                    None => self.attached_snapshot().await,
                }
            }
        };
        self.fence_codex_discovery(found).await
    }

    /// Cached catalogs must not advertise a Codex connection that has since
    /// closed. This is a local readiness read, never a reconnect or retry.
    async fn fence_codex_discovery(
        &self,
        mut found: crate::discovery::AgentPlaneDiscovery,
    ) -> crate::discovery::AgentPlaneDiscovery {
        if self
            .manager_for_agent("codex")
            .await
            .is_none_or(|manager| !manager.stream_connected())
        {
            if let Some(agent) = found.agents.iter_mut().find(|agent| {
                agent.id == "codex"
                    && agent.status == crate::discovery::AgentAvailability::Connected
            }) {
                agent.status = crate::discovery::AgentAvailability::Offline;
                agent.endpoint = None;
                agent.models.clear();
                agent.features = codex::connection_features(None, &[]);
                crate::discovery::order_agents(&mut found.agents);
            }
        }
        found
    }

    /// Discovery without touching the network: what is attached, and the
    /// configuration, nothing more.
    async fn attached_snapshot(&self) -> crate::discovery::AgentPlaneDiscovery {
        use crate::discovery::{
            AgentAvailability, AgentDiscoveryInfo, AgentPlaneDiscovery, AgentPlaneFeatures,
        };
        let attached = self.attached_in_order().await;
        let mut agents = AGENT_PREFERENCE
            .iter()
            .map(|id| {
                let (enabled, name, features) = match *id {
                    "opencode" => (
                        self.config.enabled,
                        "OpenCode",
                        opencode_features(false, false),
                    ),
                    "t3" => (self.t3.wanted(), "T3 Code", self.t3.features()),
                    "codex" => (self.codex.config.enabled, "Codex", codex::features()),
                    _ => (
                        self.deepseek_config.enabled || self.deepseek_config.endpoint.is_some(),
                        "DeepSeek",
                        deepseek_features(false, false),
                    ),
                };
                let manager = attached
                    .iter()
                    .find(|(h, _)| h == id)
                    .map(|(_, m)| m)
                    .filter(|manager| *id != "codex" || manager.stream_connected());
                let features = if *id == "codex" {
                    codex::connection_features(manager.map(|m| m.endpoint_url()), &[])
                } else {
                    features
                };
                AgentDiscoveryInfo {
                    id: id.to_string(),
                    name: name.to_string(),
                    kind: id.to_string(),
                    status: match (enabled, manager) {
                        (false, _) => AgentAvailability::Disabled,
                        (true, Some(_)) => AgentAvailability::Connected,
                        (true, None) => AgentAvailability::Offline,
                    },
                    enabled,
                    endpoint: manager.map(|m| m.endpoint_url().to_string()),
                    version: manager.and_then(|m| m.version()),
                    reason: None,
                    models: Vec::new(),
                    modes: Vec::new(),
                    features,
                }
            })
            .collect::<Vec<_>>();
        crate::discovery::order_agents(&mut agents);
        let supported = agents
            .iter()
            .any(|h| h.status == AgentAvailability::Connected);
        AgentPlaneDiscovery {
            supported,
            agents,
            features: AgentPlaneFeatures {
                multi_agent: true,
                catalog_aggregation: true,
                session_routing: true,
            },
        }
    }

    /// One discovery pass, uncached and with no TTL or timeout. The routes go
    /// through `discover_agents`; this is for the `agent` CLI only.
    pub(crate) async fn probe_agents(&self) -> crate::discovery::AgentPlaneDiscovery {
        let env_endpoint = deepseek::environment_endpoint(|key| {
            std::env::var_os(key).map(|value| value.into_string().unwrap_or_default())
        });
        self.probe_agents_with_deepseek_override(env_endpoint.as_deref())
            .await
    }

    async fn probe_agents_with_deepseek_override(
        &self,
        env_endpoint: Option<&str>,
    ) -> crate::discovery::AgentPlaneDiscovery {
        use crate::discovery::{
            AgentAvailability, AgentDiscoveryInfo, AgentPlaneDiscovery, AgentPlaneFeatures,
        };

        let mut agents = Vec::new();

        // 1. DeepSeek discovery. Read-only: never start a harness here, even
        // when autostart is on. `npx --yes` / `bunx` download when executed.
        let deepseek_enabled =
            self.deepseek_config.enabled || self.deepseek_config.endpoint.is_some();
        let deepseek_info = if !deepseek_enabled {
            AgentDiscoveryInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek".to_string(),
                kind: "deepseek".to_string(),
                status: AgentAvailability::Disabled,
                enabled: false,
                endpoint: self.deepseek_config.endpoint.clone(),
                version: None,
                reason: None,
                models: Vec::new(),
                modes: Vec::new(),
                features: deepseek_features(false, false),
            }
        } else if let Some(m) = self.manager_for_agent("deepseek").await {
            let (models, modes) = catalog_entry(m.agent().as_ref()).await;
            let supports_reasoning = models
                .iter()
                .any(|m| m.supports_reasoning || !m.reasoning_effort_tiers.is_empty());
            let has_models = !models.is_empty();

            AgentDiscoveryInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek".to_string(),
                kind: "deepseek".to_string(),
                status: AgentAvailability::Connected,
                enabled: true,
                endpoint: Some(m.endpoint_url().to_string()),
                version: m.version(),
                reason: None,
                features: deepseek_features(supports_reasoning, has_models),
                models,
                modes,
            }
        } else {
            let ep = if let Some(url) = self.deepseek_config.endpoint.as_deref().or(env_endpoint) {
                self.normalized_deepseek_endpoint(url)
            } else {
                crate::agents::adapters::deepseek::DeepseekEndpoint::discover().await
            };

            let (status, endpoint_url, version, models, modes) = match ep {
                Some(endpoint) => {
                    let client = probe_client();
                    if endpoint.probe_healthy(&client).await {
                        let driver = crate::agents::adapters::deepseek::DeepseekDriver::new(
                            endpoint.clone(),
                        );
                        let (models, modes) = catalog_entry(&driver).await;
                        (
                            AgentAvailability::Reachable,
                            Some(endpoint.url),
                            endpoint.version,
                            models,
                            modes,
                        )
                    } else {
                        (
                            AgentAvailability::Offline,
                            Some(endpoint.url),
                            None,
                            Vec::new(),
                            Vec::new(),
                        )
                    }
                }
                None => (
                    AgentAvailability::Offline,
                    None,
                    None,
                    Vec::new(),
                    Vec::new(),
                ),
            };

            let supports_reasoning = models
                .iter()
                .any(|m| m.supports_reasoning || !m.reasoning_effort_tiers.is_empty());
            let has_models = !models.is_empty();

            AgentDiscoveryInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek".to_string(),
                kind: "deepseek".to_string(),
                status,
                enabled: true,
                endpoint: endpoint_url,
                version,
                reason: None,
                features: deepseek_features(supports_reasoning, has_models),
                models,
                modes,
            }
        };
        agents.push(deepseek_info);

        // 2. OpenCode discovery
        let opencode_enabled = self.config.enabled;
        let opencode_info = if !opencode_enabled {
            AgentDiscoveryInfo {
                id: "opencode".to_string(),
                name: "OpenCode".to_string(),
                kind: "opencode".to_string(),
                status: AgentAvailability::Disabled,
                enabled: false,
                endpoint: None,
                version: None,
                reason: None,
                models: Vec::new(),
                modes: Vec::new(),
                features: opencode_features(false, false),
            }
        } else if let Some(m) = self.manager_for_agent("opencode").await {
            let (models, modes) = catalog_entry(m.agent().as_ref()).await;
            let supports_reasoning = models
                .iter()
                .any(|m| m.supports_reasoning || !m.reasoning_effort_tiers.is_empty());
            let has_models = !models.is_empty();

            AgentDiscoveryInfo {
                id: "opencode".to_string(),
                name: "OpenCode".to_string(),
                kind: "opencode".to_string(),
                status: AgentAvailability::Connected,
                enabled: true,
                endpoint: Some(m.endpoint_url().to_string()),
                version: m.version(),
                reason: None,
                features: opencode_features(supports_reasoning, has_models),
                models,
                modes,
            }
        } else {
            let (status, endpoint_url, version, reason, models, modes) =
                match OpencodeEndpoint::discover().await {
                    Some(endpoint) => {
                        let client = probe_client();
                        if let Some(endpoint) = endpoint.verified(&client).await {
                            if check_live_version(endpoint.version.as_deref()).is_err() {
                                (
                                    AgentAvailability::Unsupported,
                                    Some(endpoint.url),
                                    endpoint.version,
                                    Some("This gateway supports OpenCode 2.x only".to_string()),
                                    Vec::new(),
                                    Vec::new(),
                                )
                            } else {
                                let driver = crate::agents::adapters::opencode::OpencodeDriver::new(
                                    endpoint.clone(),
                                );
                                let (models, modes) = catalog_entry(&driver).await;
                                (
                                    AgentAvailability::Reachable,
                                    Some(endpoint.url),
                                    endpoint.version,
                                    None,
                                    models,
                                    modes,
                                )
                            }
                        } else {
                            (
                                AgentAvailability::Offline,
                                Some(endpoint.url),
                                None,
                                None,
                                Vec::new(),
                                Vec::new(),
                            )
                        }
                    }
                    None => {
                        let installed = local_installation_status(&self.config);
                        if installed == AgentInstallation::Installed {
                            (
                                AgentAvailability::Offline,
                                None,
                                None,
                                None,
                                Vec::new(),
                                Vec::new(),
                            )
                        } else {
                            (
                                AgentAvailability::NotInstalled,
                                None,
                                None,
                                None,
                                Vec::new(),
                                Vec::new(),
                            )
                        }
                    }
                };

            let supports_reasoning = models
                .iter()
                .any(|m| m.supports_reasoning || !m.reasoning_effort_tiers.is_empty());
            let has_models = !models.is_empty();

            AgentDiscoveryInfo {
                id: "opencode".to_string(),
                name: "OpenCode".to_string(),
                kind: "opencode".to_string(),
                status,
                enabled: true,
                endpoint: endpoint_url,
                version,
                reason,
                features: opencode_features(supports_reasoning, has_models),
                models,
                modes,
            }
        };
        agents.push(opencode_info);
        agents.push(self.t3_discovery().await);
        agents.push(self.codex.discovery(self).await);
        crate::discovery::order_agents(&mut agents);

        let supported = agents.iter().any(|h| {
            h.status == AgentAvailability::Connected || h.status == AgentAvailability::Reachable
        });

        AgentPlaneDiscovery {
            supported,
            agents,
            features: AgentPlaneFeatures {
                multi_agent: true,
                catalog_aggregation: true,
                session_routing: true,
            },
        }
    }

    /// Attach an agent now, and keep one attached. Safe to call once.
    pub fn spawn_supervisor(self: &Arc<Self>) {
        if self.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut start_backoff = Duration::from_secs(2);
            let mut stream_loss_wait = STREAM_LOSS_FIRST_WAIT;
            loop {
                let healthy = runtime.check_and_repair(&mut start_backoff).await;
                if healthy {
                    runtime.watch_while_healthy(&mut stream_loss_wait).await;
                } else {
                    tokio::time::sleep(UNHEALTHY_POLL).await;
                }
            }
        });
    }

    /// Hold until the agent is worth checking again.
    ///
    /// Normally that is the next health poll. But the event stream dropping is
    /// the agent telling us it has gone, and that should not wait: this
    /// returns within a tick of the stream going down, so the gap between
    /// OpenCode dying and the gateway re-attaching is about a second rather
    /// than however much of the poll interval was left.
    async fn watch_while_healthy(&self, stream_loss_wait: &mut Duration) {
        let deadline = tokio::time::Instant::now() + HEALTHY_POLL;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                // A full interval with the stream up: whatever flapping there
                // was has settled, so the next loss is looked at immediately.
                *stream_loss_wait = STREAM_LOSS_FIRST_WAIT;
                return;
            }
            tokio::time::sleep(STREAM_WATCH_TICK.min(remaining)).await;
            if self.stream_down().await {
                tracing::warn!(
                    wait_s = stream_loss_wait.as_secs(),
                    "primary agent event stream is down, re-discovering"
                );
                if !stream_loss_wait.is_zero() {
                    tokio::time::sleep(*stream_loss_wait).await;
                }
                *stream_loss_wait = next_stream_loss_wait(*stream_loss_wait);
                return;
            }
        }
    }

    /// True when an agent is attached but its event stream has dropped.
    async fn stream_down(&self) -> bool {
        self.manager
            .read()
            .await
            .as_ref()
            .map(|m| !m.stream_connected())
            .unwrap_or(false)
    }

    /// One supervision pass. Returns whether an agent is attached and well.
    async fn check_and_repair(&self, start_backoff: &mut Duration) -> bool {
        // 1. Supervise DeepSeek: adopt a healthy harness, or start one when
        // enabled and autostart is on. A missing runner does not fail the pass.
        let deepseek_ok = self.supervise_deepseek().await;

        // 2. Supervise OpenCode
        let discovered_opencode = if self.config.enabled {
            OpencodeEndpoint::discover_with_registration_id().await
        } else {
            None
        };

        // An explicit endpoint is attach-only: a failed identity, auth or
        // version check must not start an unrelated local fallback service.
        let external_endpoint =
            external_endpoint_configured_with(|name| std::env::var(name)).unwrap_or(true);
        let opencode_ok = self
            .supervise_opencode(discovered_opencode, external_endpoint, start_backoff)
            .await;

        // 3. Supervise T3 Code
        let t3_ok = self.supervise_t3().await;
        let codex_ok = self.codex.supervise(self).await;

        if deepseek_ok || opencode_ok || t3_ok || codex_ok {
            *start_backoff = Duration::from_secs(2);
            let primary = pick_primary(&*self.managers.read().await).cloned();
            if let Some(mgr) = primary {
                *self.manager.write().await = Some(mgr);
            }
            true
        } else {
            *self.manager.write().await = None;
            *self.origin.write().await = AgentOrigin::None;
            false
        }
    }

    async fn supervise_opencode(
        &self,
        discovered: Option<(OpencodeEndpoint, Option<String>)>,
        external_endpoint: bool,
        start_backoff: &mut Duration,
    ) -> bool {
        let mut healthy = self.repair_opencode(discovered).await;
        if !healthy && self.config.enabled && self.config.autostart && !external_endpoint {
            match self.start_service().await {
                Ok((endpoint, id)) => {
                    self.attach(endpoint, id, AgentOrigin::Spawned).await;
                    healthy = true;
                }
                Err(err) => {
                    tracing::warn!(%err, backoff_s = start_backoff.as_secs(), "could not start opencode");
                    tokio::time::sleep(*start_backoff).await;
                    *start_backoff = (*start_backoff * 2).min(MAX_START_BACKOFF);
                }
            }
        }
        healthy
    }

    async fn repair_opencode(
        &self,
        discovered: Option<(OpencodeEndpoint, Option<String>)>,
    ) -> bool {
        let verified = match discovered {
            Some((endpoint, id)) => endpoint
                .verified(&probe_client())
                .await
                .filter(|endpoint| check_live_version(endpoint.version.as_deref()).is_ok())
                .map(|endpoint| (endpoint, id)),
            None => None,
        };
        if let Some(manager) = self.manager_for_agent("opencode").await {
            let identity = self.opencode_instance.read().await;
            if verified.as_ref().is_some_and(|(endpoint, id)| {
                endpoint.url == manager.endpoint_url()
                    && endpoint.version == manager.version()
                    && identity.as_ref() == Some(&OpencodeInstance::new(endpoint, id.clone()))
            }) {
                return true;
            }
            drop(identity);
            tracing::warn!("OpenCode agent unhealthy or instance changed, re-discovering");
            manager.shutdown();
            self.managers.write().await.remove("opencode");
            *self.opencode_instance.write().await = None;
            let mut primary = self.manager.write().await;
            if primary.as_ref().is_some_and(|p| Arc::ptr_eq(p, &manager)) {
                *primary = None;
                *self.origin.write().await = AgentOrigin::None;
            }
        }
        if let Some((endpoint, id)) = verified {
            self.attach(endpoint, id, AgentOrigin::Adopted).await;
            true
        } else {
            false
        }
    }

    async fn attach(
        &self,
        endpoint: OpencodeEndpoint,
        registration_id: Option<String>,
        origin: AgentOrigin,
    ) {
        let instance = OpencodeInstance::new(&endpoint, registration_id);
        let url = endpoint.url.clone();
        let version = endpoint.version.clone();
        // Which file is actually serving this. For a service this gateway
        // started that is the path it resolved; for one it adopted it is read
        // off the running process, because "which opencode am I talking to" is
        // the question an operator has after a restart and a bare `opencode`
        // does not answer it.
        let path = endpoint
            .pid
            .and_then(running_binary_path)
            .map(|p| p.display().to_string());
        let manager = AgentManager::connect(endpoint, self.events_tx.clone());
        self.install("opencode", manager, origin).await;
        // Publish identity only after installing its manager. Cancellation
        // before this write causes a safe reattachment, never old-client reuse.
        *self.opencode_instance.write().await = Some(instance);
        let path = path.unwrap_or_else(|| "unknown".to_string());
        match origin {
            AgentOrigin::Adopted => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                binary = %path,
                "adopted the running OpenCode service"
            ),
            AgentOrigin::Spawned => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                binary = %path,
                "started an OpenCode service and attached to it"
            ),
            AgentOrigin::None => {}
        }
    }

    /// Register a freshly connected manager under its agent id and let the
    /// preference order decide whether it is now the primary. Every attach
    /// path ends here.
    async fn install(&self, agent_id: &str, manager: AgentManager, origin: AgentOrigin) {
        let replaced = self
            .managers
            .write()
            .await
            .insert(agent_id.to_string(), Arc::new(manager));
        if let Some(old) = replaced {
            old.shutdown();
        }
        self.promote_primary(agent_id, origin).await;
    }

    /// Whether the attached manager for `agent_id` still answers its probe.
    /// One that does not is shut down and detached, so the next pass looks
    /// for the agent afresh. `false` when nothing is attached.
    async fn keep_if_healthy(&self, agent_id: &str) -> bool {
        let Some(manager) = self.managers.read().await.get(agent_id).cloned() else {
            return false;
        };
        if manager.agent().probe().await.unwrap_or(false) {
            return true;
        }
        tracing::warn!(
            agent_id,
            url = manager.endpoint_url(),
            "agent unhealthy, re-probing"
        );
        manager.shutdown();
        self.managers.write().await.remove(agent_id);
        false
    }

    /// Make the most preferred attached manager the primary; `origin` is
    /// recorded when that is the one that was just attached.
    async fn promote_primary(&self, attached: &str, origin: AgentOrigin) {
        let primary = {
            let map = self.managers.read().await;
            AGENT_PREFERENCE
                .iter()
                .find(|id| map.contains_key(**id))
                .map(|id| (*id, map[*id].clone()))
        };
        if let Some((id, manager)) = primary {
            *self.manager.write().await = Some(manager);
            if id == attached {
                *self.origin.write().await = origin;
            }
        }
    }

    async fn attach_deepseek(
        &self,
        endpoint: crate::agents::adapters::deepseek::DeepseekEndpoint,
        origin: AgentOrigin,
    ) {
        let url = endpoint.url.clone();
        let version = endpoint.version.clone();
        let manager = AgentManager::connect_deepseek(endpoint, self.events_tx.clone());
        self.install("deepseek", manager, origin).await;
        *self.deepseek_origin.write().await = origin;
        match origin {
            AgentOrigin::Adopted => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                "adopted the running DeepSeek Harness"
            ),
            AgentOrigin::Spawned => tracing::info!(
                url = %url,
                version = version.as_deref().unwrap_or("unknown"),
                "started DeepSeek Harness and attached to it"
            ),
            AgentOrigin::None => {}
        }
    }

    async fn probe(&self, url: &str) -> bool {
        let Some(manager) = self.manager.read().await.clone() else {
            return false;
        };
        if manager.endpoint_url() != url {
            return false;
        }
        manager.agent().probe().await.unwrap_or(false)
    }

    /// Let OpenCode load its saved service configuration and start or reuse
    /// its background server, then verify the registered endpoint.
    async fn start_service(&self) -> anyhow::Result<(OpencodeEndpoint, Option<String>)> {
        let _start = self.start_lock.lock().await;

        // `opencode.binary` if the owner set one, otherwise whatever `opencode`
        // means on PATH -- and then the file that resolved to, so the log names
        // a path rather than a word.
        let binary = match resolve_binary(self.config.binary.as_deref()) {
            Ok(path) => path,
            Err(err) => {
                anyhow::bail!("{err}");
            }
        };
        let version = binary_version(&binary).await;
        if let Err(refusal) = check_version(version.as_deref()) {
            // One line, naming the file and what it said, because the reader
            // has to go and fix an install.
            tracing::error!("refusing to start {}: {refusal}", binary.display());
            anyhow::bail!("{} is not OpenCode 2.x", binary.display());
        }
        tracing::info!(
            binary = %binary.display(),
            version = version.as_deref().unwrap_or("unknown"),
            "no OpenCode service found, starting one"
        );

        let mcp = self.opencode_mcp_env().await;
        let started = async {
            launch_service(
                tokio::process::Command::new(&binary),
                mcp.as_ref().map(|(env, _)| env),
            )
            .await?;
            wait_for_service().await
        }
        .await;
        if let (Some((env, previous)), Some(wiring)) = (&mcp, self.mcp.get()) {
            settle_agent_tokens(wiring.tokens.as_ref(), env, previous, started.is_ok()).await;
        }
        started
    }
}

/// Once the start outcome is known: on success the earlier agent tokens are
/// revoked; on failure the token just minted is, and the earlier ones stay.
async fn settle_agent_tokens(
    tokens: &dyn crate::mcp::tokens::AssistantTokenStore,
    minted: &McpEnv,
    previous: &[String],
    started: bool,
) {
    let doomed: &[String] = if started {
        previous
    } else {
        std::slice::from_ref(&minted.token_id)
    };
    for id in doomed {
        if let Err(err) = tokens.revoke(id).await {
            tracing::warn!("could not revoke an OpenCode MCP token: {err}");
        }
    }
}

/// Mint a new OpenCode token; the ids of the live earlier ones come back so
/// they can be revoked once the new one is in use.
async fn issue_agent_env(
    tokens: &dyn crate::mcp::tokens::AssistantTokenStore,
    url: &str,
) -> anyhow::Result<(McpEnv, Vec<String>)> {
    use crate::mcp::tokens::{TokenBinding, NO_EXPIRY};
    let binding = TokenBinding::Agent {
        agent_id: "opencode".into(),
    };
    let previous = tokens
        .list()
        .await?
        .into_iter()
        .filter(|row| row.binding == binding)
        .map(|row| row.id)
        .collect();
    let issued = tokens.issue(binding, "opencode".into(), NO_EXPIRY).await?;
    let env = McpEnv {
        token_id: issued.session.token_id,
        token: issued.token,
        url: url.to_owned(),
    };
    Ok((env, previous))
}

/// `opencode service set env NAME VALUE`: the daemon loads its saved service
/// env, not the environment of the process that starts it.
fn service_set_args(name: &str, value: &str) -> [String; 5] {
    ["service", "set", "env", name, value].map(str::to_owned)
}

/// A command for the same program, environment and directory as `like`.
fn sibling_command(like: &tokio::process::Command) -> tokio::process::Command {
    let like = like.as_std();
    let mut command = tokio::process::Command::new(like.get_program());
    for (name, value) in like.get_envs() {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    if let Some(dir) = like.get_current_dir() {
        command.current_dir(dir);
    }
    command
}

/// Hand the daemon its MCP env through its saved service configuration. A
/// failure is logged and the start goes on without it.
async fn save_service_env(like: &tokio::process::Command, mcp: &McpEnv) {
    for (name, value) in mcp.vars() {
        let mut command = sibling_command(like);
        command
            .args(service_set_args(name, value))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(STARTUP_WAIT, command.status()).await {
            Ok(Ok(status)) if status.success() => {}
            _ => {
                tracing::warn!("`opencode service set env {name}` failed; starting without it");
                return;
            }
        }
    }
}

async fn launch_service(
    mut command: tokio::process::Command,
    mcp: Option<&McpEnv>,
) -> anyhow::Result<()> {
    if let Some(mcp) = mcp {
        save_service_env(&command, mcp).await;
        mcp.apply(&mut command);
    }
    // `serve --service` bypasses saved service env; the CLI owns that setup.
    command
        .args(["service", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let status = tokio::time::timeout(STARTUP_WAIT, command.status())
        .await
        .map_err(|_| anyhow::anyhow!("`opencode service start` timed out"))?
        .map_err(|err| anyhow::anyhow!("could not run `opencode service start`: {err}"))?;
    anyhow::ensure!(
        status.success(),
        "`opencode service start` exited with {status}"
    );
    Ok(())
}

/// The wait after a stream loss, given the last one.
///
/// The first loss is looked at immediately, because that is the common case
/// and the whole point. Repeated losses without a settled interval in between
/// mean something is flapping, and the supervisor backs off rather than
/// re-discovering once a second forever.
fn next_stream_loss_wait(current: Duration) -> Duration {
    if current.is_zero() {
        Duration::from_secs(1)
    } else {
        (current * 2).min(STREAM_LOSS_MAX_WAIT)
    }
}

/// The major version out of whatever `--version` or a registration said:
/// `opencode v2.0.1`, `2.0.1`, `v2.0.1-beta.3`.
fn parse_major(version: &str) -> Option<u64> {
    version
        .split_whitespace()
        .filter_map(|word| {
            let word = word.trim_start_matches(['v', 'V']);
            let core = word.split(['-', '+']).next()?;
            let mut parts = core.split('.');
            let major = parts.next()?.parse::<u64>().ok()?;
            if parts.any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
            {
                return None;
            }
            Some((major, word))
        })
        // A bare `2` is not a version string; take the first word that looks
        // like one, so `opencode v2.0.1` is not read as the `opencode` in it.
        .find(|(_, word)| word.contains('.'))
        .map(|(major, _)| major)
        .or_else(|| {
            version
                .trim()
                .trim_start_matches(['v', 'V'])
                .parse::<u64>()
                .ok()
        })
}

/// Whether a binary's reported version permits trying startup.
///
/// An unreadable or absent version is allowed through: it cannot be shown to
/// be incompatible, and refusing on silence would break an install that simply
/// does not report one. The live server must still report a supported version.
fn check_version(version: Option<&str>) -> Result<(), String> {
    let Some(version) = version.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let Some(major) = parse_major(version) else {
        return Ok(());
    };
    if major == SUPPORTED_OPENCODE_MAJOR {
        return Ok(());
    }
    Err(format!(
        "this gateway speaks OpenCode \
         {SUPPORTED_OPENCODE_MAJOR}.x only. Install a supported OpenCode version, or point the gateway \
         at the right one by setting `opencode.binary` to its absolute path in \
         config.json"
    ))
}

/// Unlike a binary flag probe, authenticated live info must establish a known
/// supported major. Preserve the existing version-prefix parsing convention.
fn check_live_version(version: Option<&str>) -> Result<(), String> {
    if version.and_then(parse_major) == Some(SUPPORTED_OPENCODE_MAJOR) {
        Ok(())
    } else {
        Err("This gateway supports OpenCode 2.x only".to_string())
    }
}

/// `opencode.binary` if the owner set one, else `opencode` as `PATH` resolves
/// it -- and in both cases the file it actually is.
///
/// No install directory is guessed at. Where OpenCode lives differs per OS and
/// per install, and a gateway reaching into one of its own would quietly run a
/// different binary than the owner's shell does, which is the confusion this
/// is here to end.
pub(crate) fn resolve_binary(configured: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    resolve_binary_in(configured, std::env::var_os("PATH").as_deref())
}

/// The lookup itself, with `PATH` passed in rather than read, so a test can
/// exercise the order without reaching into the process environment that every
/// other thread is also using.
fn resolve_binary_in(
    configured: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
) -> anyhow::Result<std::path::PathBuf> {
    let name = binary_name(configured);

    // Anything with a separator is a path the owner meant literally, and a
    // missing one is an error naming it rather than a quiet fall back to PATH:
    // they asked for that file.
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        let path = std::path::PathBuf::from(name);
        if !path.is_file() {
            anyhow::bail!("`opencode.binary` is set to {name}, and there is no file there");
        }
        return Ok(absolute(path));
    }

    let path_var = path_var
        .ok_or_else(|| anyhow::anyhow!("PATH is not set, so `{name}` cannot be resolved"))?;
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Ok(absolute(candidate));
        }
    }
    anyhow::bail!(
        "`{name}` is not on PATH. Install OpenCode 2, or set `opencode.binary` \
         to its absolute path in config.json"
    )
}

fn binary_name(configured: Option<&str>) -> &str {
    configured
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("opencode")
}

/// Classify installation without starting OpenCode or contacting an endpoint.
/// The inputs are injected to keep lookup tests independent of global process
/// environment.
fn installation_status_in(
    configured: Option<&str>,
    path_var: Option<&std::ffi::OsStr>,
    external_endpoint_configured: bool,
) -> AgentInstallation {
    if external_endpoint_configured {
        return AgentInstallation::Unknown;
    }

    let name = binary_name(configured);
    if name.contains(std::path::MAIN_SEPARATOR) || name.contains('/') {
        return match std::fs::metadata(name) {
            Ok(metadata) if metadata_is_executable(&metadata) => AgentInstallation::Installed,
            Ok(_) => AgentInstallation::NotFound,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => AgentInstallation::NotFound,
            Err(_) => AgentInstallation::Unknown,
        };
    }

    let Some(path_var) = path_var else {
        return AgentInstallation::Unknown;
    };
    let mut ambiguous = false;
    for directory in std::env::split_paths(path_var) {
        let candidate = directory.join(name);
        match std::fs::metadata(&candidate) {
            Ok(metadata) if metadata_is_executable(&metadata) => {
                return AgentInstallation::Installed;
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => ambiguous = true,
        }
    }
    if ambiguous {
        AgentInstallation::Unknown
    } else {
        AgentInstallation::NotFound
    }
}

fn local_installation_status(config: &OpencodeConfig) -> AgentInstallation {
    if !config.enabled {
        return AgentInstallation::NotFound;
    }
    let Ok(external_endpoint_configured) =
        external_endpoint_configured_with(|name| std::env::var(name))
    else {
        return AgentInstallation::Unknown;
    };
    installation_status_in(
        config.binary.as_deref(),
        std::env::var_os("PATH").as_deref(),
        external_endpoint_configured,
    )
}

fn external_endpoint_configured_with(
    mut read: impl FnMut(&str) -> Result<String, std::env::VarError>,
) -> Result<bool, ()> {
    for name in ["OPENCODE_URL", "HERDR_GATEWAY_OPENCODE_URL"] {
        match read(name) {
            Ok(url) => return Ok(!url.trim().is_empty()),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => return Err(()),
        }
    }
    Ok(false)
}

fn absolute(path: std::path::PathBuf) -> std::path::PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
}

fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata_is_executable(&metadata))
        .unwrap_or(false)
}

/// What `<binary> --version` says, or `None` if it cannot be asked.
/// Prefer the first nonempty stdout line, then stderr. Unlike the old probe,
/// leading blank lines do not hide a version that follows them.
pub(crate) async fn binary_version(path: &std::path::Path) -> Option<String> {
    use tokio::io::AsyncReadExt as _;

    let mut child = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdout = child.stdout.take().expect("piped version stdout");
    let stderr = child.stderr.take().expect("piped version stderr");
    // Read both pipes concurrently: either one can fill while the other is idle.
    // One extra byte detects overflow without retaining an unbounded response.
    async fn read(stream: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        stream
            .take(VERSION_OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() as u64 > VERSION_OUTPUT_LIMIT {
            return Err(std::io::Error::other("version output limit exceeded"));
        }
        Ok(bytes)
    }
    let result = tokio::time::timeout(VERSION_PROBE_TIMEOUT, async {
        let (stdout, stderr) = tokio::try_join!(read(stdout), read(stderr))?;
        // Preserve version reporting even for executables that exit nonzero.
        child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stderr))
    })
    .await;
    let (stdout, stderr) = match result {
        Ok(Ok(output)) => output,
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
    };
    [stdout, stderr].into_iter().find_map(|bytes| {
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    })
}

/// The executable behind a running pid, where the platform will say.
#[cfg(target_os = "linux")]
fn running_binary_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(not(target_os = "linux"))]
fn running_binary_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
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
async fn wait_for_service() -> anyhow::Result<(OpencodeEndpoint, Option<String>)> {
    let client = probe_client();
    let deadline = std::time::Instant::now() + STARTUP_WAIT;
    loop {
        if let Some((endpoint, id)) = OpencodeEndpoint::discover_with_registration_id().await {
            if let Some(endpoint) = endpoint.verified(&client).await {
                check_live_version(endpoint.version.as_deref()).map_err(anyhow::Error::msg)?;
                return Ok((endpoint, id));
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

fn deepseek_features(
    supports_reasoning: bool,
    has_models: bool,
) -> crate::discovery::AgentFeatures {
    crate::discovery::AgentFeatures {
        streaming: true,
        reasoning_effort: supports_reasoning,
        model_selection: has_models,
        tool_approvals: true,
        worktrees: false,
        revert: false,
        staged_revert: false,
        inbox: false,
        // Presets are the modes; attachments arrive as host paths in the
        // prompt text, and the driver leaves skills, commands, compaction and
        // shells at `Unsupported`.
        modes: true,
        skills: false,
        slash_commands: false,
        compaction: false,
        background_shells: false,
        attachments: true,
        attachments_by_path: true,
        extra: std::collections::BTreeMap::from([
            ("modeSwitching".into(), serde_json::Value::Bool(false)),
            ("sessionRename".into(), serde_json::json!(true)),
            ("sessionDelete".into(), serde_json::json!(false)),
            ("sessionFork".into(), serde_json::json!(true)),
            ("sessionPagination".into(), serde_json::Value::Bool(false)),
            ("timelineEarlier".into(), serde_json::json!(true)),
        ]),
    }
}

fn opencode_features(
    supports_reasoning: bool,
    has_models: bool,
) -> crate::discovery::AgentFeatures {
    crate::discovery::AgentFeatures {
        streaming: true,
        reasoning_effort: supports_reasoning,
        model_selection: has_models,
        tool_approvals: true,
        worktrees: true,
        revert: true,
        staged_revert: true,
        inbox: true,
        modes: true,
        skills: true,
        slash_commands: true,
        compaction: true,
        background_shells: true,
        attachments: true,
        attachments_by_path: false,
        extra: std::collections::BTreeMap::from([
            ("sessionPagination".into(), serde_json::json!(true)),
            ("timelineEarlier".into(), serde_json::json!(true)),
            ("sessionRename".into(), serde_json::json!(true)),
            ("sessionDelete".into(), serde_json::json!(true)),
            ("sessionFork".into(), serde_json::json!(false)),
        ]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockOpencode {
        endpoint: OpencodeEndpoint,
        info: Arc<RwLock<(axum::http::StatusCode, serde_json::Value)>>,
        server: tokio::task::JoinHandle<()>,
    }

    impl MockOpencode {
        async fn new() -> Self {
            use axum::{
                extract::State,
                http::{HeaderMap, StatusCode},
                routing::get,
                Json, Router,
            };
            let info = Arc::new(RwLock::new((
                StatusCode::OK,
                serde_json::json!({
                    "version":"2.0.27", "pid":123, "urls":["http://localhost"], "paths":{"tmp":"/fixture"}
                }),
            )));
            let app = Router::new()
                .route(
                    "/api/info",
                    get(
                        |State(info): State<Arc<RwLock<(StatusCode, serde_json::Value)>>>,
                         headers: HeaderMap| async move {
                            if headers.get("authorization").and_then(|h| h.to_str().ok())
                                != Some("Basic b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==")
                            {
                                return (StatusCode::UNAUTHORIZED, Json(serde_json::Value::Null));
                            }
                            let (status, body) = info.read().await.clone();
                            (status, Json(body))
                        },
                    ),
                )
                .with_state(info.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = OpencodeEndpoint {
                url: format!("http://{}", listener.local_addr().unwrap()),
                password: Some("fixture-password".into()),
                version: None,
                pid: None,
            };
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Self {
                endpoint,
                info,
                server,
            }
        }
    }

    impl Drop for MockOpencode {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    #[tokio::test]
    async fn opencode_reuses_only_the_same_verified_instance_and_reports_actual_info() {
        let service = MockOpencode::new().await;
        let runtime = AgentRuntime::new(OpencodeConfig {
            autostart: false,
            ..Default::default()
        });
        let mut id = Some("fixture-instance-a".to_string());
        assert!(
            runtime
                .repair_opencode(Some((service.endpoint.clone(), id.clone())))
                .await
        );
        let first = runtime.manager_for_agent("opencode").await.unwrap();
        assert_eq!(runtime.status().await.version.as_deref(), Some("2.0.27"));
        assert_eq!(
            runtime.opencode_instance.read().await.as_ref().unwrap().pid,
            Some(123)
        );
        assert!(
            runtime
                .repair_opencode(Some((service.endpoint.clone(), id.clone())))
                .await
        );
        assert!(Arc::ptr_eq(
            &first,
            &runtime.manager_for_agent("opencode").await.unwrap()
        ));

        let mut previous = first;
        for (version, pid, registration_id) in [
            ("2.0.27", 456, "fixture-instance-a"),
            ("2.0.99", 456, "fixture-instance-a"),
            ("2.0.99", 456, "fixture-instance-b"),
        ] {
            service.info.write().await.1 =
                serde_json::json!({"version":version,"pid":pid,"urls":["http://localhost"]});
            id = Some(registration_id.into());
            assert!(
                runtime
                    .repair_opencode(Some((service.endpoint.clone(), id.clone())))
                    .await
            );
            let current = runtime.manager_for_agent("opencode").await.unwrap();
            assert!(!Arc::ptr_eq(&previous, &current));
            assert_eq!(runtime.status().await.version.as_deref(), Some(version));
            previous = current;
        }

        let mut stale = service.endpoint.clone();
        stale.pid = Some(456);
        stale.version = Some("2.0.27".into());
        assert!(!runtime.repair_opencode(Some((stale, id.clone()))).await);
        assert!(runtime.manager_for_agent("opencode").await.is_none());
        assert!(runtime.opencode_instance.read().await.is_none());
        assert!(
            runtime
                .repair_opencode(Some((service.endpoint.clone(), id)))
                .await
        );
        let mut stale = service.endpoint.clone();
        stale.pid = Some(123);
        assert!(!runtime.repair_opencode(Some((stale, None))).await);
        assert!(runtime.manager_for_agent("opencode").await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_explicit_opencode_never_attaches_or_starts_a_fallback() {
        use axum::http::StatusCode;
        let service = MockOpencode::new().await;
        let binary = VersionScript::new("printf '2.0.27\\n'");
        let runtime = AgentRuntime::new(OpencodeConfig {
            autostart: true,
            binary: Some(binary.0.to_str().unwrap().into()),
            ..Default::default()
        });
        let mut backoff = Duration::from_secs(2);
        for (status, body) in [
            (StatusCode::UNAUTHORIZED, serde_json::Value::Null),
            (StatusCode::FORBIDDEN, serde_json::Value::Null),
            (StatusCode::SERVICE_UNAVAILABLE, serde_json::Value::Null),
            (StatusCode::NOT_FOUND, serde_json::Value::Null),
            (StatusCode::OK, serde_json::json!({"healthy":true})),
            (
                StatusCode::OK,
                serde_json::json!({"version":"unknown","pid":123,"urls":["http://localhost"]}),
            ),
            (
                StatusCode::OK,
                serde_json::json!({"version":"1.18.4","pid":123,"urls":["http://localhost"]}),
            ),
            (
                StatusCode::OK,
                serde_json::json!({"version":"3.0.0","pid":123,"urls":["http://localhost"]}),
            ),
        ] {
            *service.info.write().await = (status, body);
            assert!(
                !runtime
                    .supervise_opencode(Some((service.endpoint.clone(), None)), true, &mut backoff)
                    .await
            );
            assert!(runtime.manager_for_agent("opencode").await.is_none());
        }
        assert!(!runtime.supervise_opencode(None, true, &mut backoff).await);
        assert_eq!(backoff, Duration::from_secs(2));
        assert!(
            !binary.0.with_file_name("pid").exists(),
            "even the fallback binary version probe must not run"
        );
    }

    #[test]
    fn live_opencode_requires_a_known_supported_version_but_binary_silence_is_allowed() {
        for version in [
            None,
            Some(""),
            Some("unknown"),
            Some("2.invalid"),
            Some("1.18.4"),
            Some("3.0.0"),
        ] {
            assert!(check_live_version(version).is_err());
        }
        for version in ["2.0.27", "2.0.99", "opencode v2.0.27", "v2.0.27-beta.3"] {
            assert!(check_live_version(Some(version)).is_ok());
        }
        assert!(check_version(None).is_ok());
        assert!(check_version(Some("unknown build")).is_ok());
    }

    #[test]
    fn opencode_credential_rotation_changes_the_reuse_identity() {
        let mut endpoint = OpencodeEndpoint {
            url: "http://localhost".into(),
            password: Some("fixture-old-password".into()),
            version: Some("2.0.27".into()),
            pid: Some(123),
        };
        let old = OpencodeInstance::new(&endpoint, None);
        endpoint.password = Some("fixture-new-password".into());
        assert!(old != OpencodeInstance::new(&endpoint, None));
    }

    #[tokio::test]
    async fn opencode_probe_logs_do_not_capture_credentials_or_invalid_response_bodies() {
        use tracing::instrument::WithSubscriber as _;
        // With one live dispatch, tracing's global callsite cache is rebuilt
        // using the registering thread's default. Another test can register
        // our shared log callsite under NoSubscriber and suppress our capture.
        // A second live dispatch forces registry-wide interest calculation.
        let _interest_guard = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        #[derive(Clone)]
        struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let capture = Capture(Arc::default());
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        async {
            let service = MockOpencode::new().await;
            let runtime = AgentRuntime::new(OpencodeConfig {
                autostart: false,
                ..Default::default()
            });
            assert!(
                runtime
                    .repair_opencode(Some((service.endpoint.clone(), None)))
                    .await
            );
            service.info.write().await.1 = serde_json::json!({
                "version":"fixture-private-response", "pid":123, "urls":["http://localhost"]
            });
            assert!(
                !runtime
                    .repair_opencode(Some((service.endpoint.clone(), None)))
                    .await
            );
        }
        .with_subscriber(subscriber)
        .await;
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("adopted the running OpenCode service"));
        assert!(log.contains("instance changed"));
        for secret in [
            "fixture-password",
            "b3BlbmNvZGU6Zml4dHVyZS1wYXNzd29yZA==",
            "fixture-private-response",
        ] {
            assert!(
                !log.contains(secret),
                "sensitive fixture data leaked to logs"
            );
        }
    }

    #[cfg(unix)]
    struct VersionScript(std::path::PathBuf);

    #[cfg(unix)]
    impl VersionScript {
        fn new(body: &str) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let root = std::env::temp_dir().join(format!("muqun-version-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let path = root.join("probe");
            let pid_path = root.join("pid");
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\ntest \"$1\" = --fixture-ready && exit 0\ntest \"$1\" = --version || exit 99\nprintf '%s' \"$$\" > '{}'\n{body}\n",
                    pid_path.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            // A concurrent fork can retain our already-closed writable fd
            // until its exec, temporarily causing ETXTBSY. The readiness arm
            // exits before recording a pid or running the probe body. Once
            // executable, this immutable fixture cannot become busy again.
            let deadline = std::time::Instant::now() + VERSION_PROBE_TIMEOUT;
            loop {
                match std::process::Command::new(&path)
                    .arg("--fixture-ready")
                    .status()
                {
                    Ok(status) => {
                        assert!(status.success(), "version fixture readiness failed");
                        break;
                    }
                    Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "version fixture remained busy"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!(
                        "version fixture readiness spawn failed: errno={:?}",
                        error.raw_os_error()
                    ),
                }
            }
            Self(path)
        }

        fn assert_reaped(&self) {
            let pid: libc::pid_t = std::fs::read_to_string(self.0.with_file_name("pid"))
                .unwrap()
                .parse()
                .unwrap();
            // Only this fixture's direct child: an exited but unreaped child
            // would return its pid, and a still-running one would return zero.
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }

    #[cfg(unix)]
    impl Drop for VersionScript {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn version_fixture_readiness_is_side_effect_free() {
        for _ in 0..8 {
            // The body fails if readiness has already run it.
            let script = VersionScript::new(
                "test ! -f \"$0.body-ran\" || exit 99; printf 'ran' > \"$0.body-ran\"; printf 'fixture-version\\n'",
            );
            assert!(!script.0.with_file_name("pid").exists());
            assert!(!script.0.with_extension("body-ran").exists());
            assert_eq!(
                binary_version(&script.0).await.as_deref(),
                Some("fixture-version")
            );
            assert!(script.0.with_extension("body-ran").exists());
            script.assert_reaped();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_preserves_stdout_and_stderr_versions() {
        for (body, expected) in [
            (
                "printf 'opencode 2.1.0\\nignored\\n'; printf 'diagnostic\\n' >&2",
                Some("opencode 2.1.0"),
            ),
            (
                "printf ' \\n\\n'; printf '\\n opencode 2.2.0 \\n' >&2; exit 7",
                Some("opencode 2.2.0"),
            ),
            (
                "printf '\\n opencode 2.3.0 \\n'; printf 'diagnostic\\n' >&2",
                Some("opencode 2.3.0"),
            ),
            ("exit 0", None),
        ] {
            let script = VersionScript::new(body);
            assert_eq!(binary_version(&script.0).await.as_deref(), expected);
            script.assert_reaped();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_kills_and_reaps_output_floods() {
        for redirect in ["", ">&2"] {
            let script = VersionScript::new(&format!(
                "while :; do printf '0123456789abcdef0123456789abcdef' {redirect}; done"
            ));
            let start = std::time::Instant::now();
            assert_eq!(binary_version(&script.0).await, None);
            assert!(start.elapsed() < VERSION_PROBE_TIMEOUT);
            script.assert_reaped();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_accepts_exact_output_limit_but_rejects_one_extra_byte() {
        for redirect in ["", ">&2"] {
            for bytes in [VERSION_OUTPUT_LIMIT, VERSION_OUTPUT_LIMIT + 1] {
                let output = "x".repeat(bytes as usize);
                let script = VersionScript::new(&format!("printf '%s' '{output}' {redirect}"));
                let expected = (bytes == VERSION_OUTPUT_LIMIT).then_some(output);
                assert_eq!(binary_version(&script.0).await, expected);
                script.assert_reaped();
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn binary_version_abort_kills_child_and_tokio_eventually_reaps_it() {
        let script = VersionScript::new("while :; do :; done");
        let path = script.0.clone();
        let probe = tokio::spawn(async move { binary_version(&path).await });
        // Wait for this fixture alone to report its pid. Partial file writes
        // are retried, and an unsuccessful rendezvous still aborts the probe.
        let ready = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(script.0.with_file_name("pid"))
                    .ok()
                    .and_then(|pid| pid.parse::<libc::pid_t>().ok())
                    .filter(|pid| *pid > 0)
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        probe.abort();
        assert!(probe.await.unwrap_err().is_cancelled());
        let pid = ready.expect("version child reports its pid before cancellation");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // WNOWAIT observes only our direct child without reaping it:
                // ECHILD therefore proves Tokio, not this test, reaped it.
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ECHILD)
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled version child is killed and reaped by Tokio");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn binary_version_times_out_without_stalling_runtime_heartbeat() {
        // Shell builtins only: no descendant process to leak from the fixture.
        let script = VersionScript::new("while :; do :; done");
        let start = std::time::Instant::now();
        let (version, ()) = tokio::join!(binary_version(&script.0), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(start.elapsed() < Duration::from_secs(1));
        });
        assert_eq!(version, None);
        assert!(start.elapsed() < VERSION_PROBE_TIMEOUT + Duration::from_secs(2));
        script.assert_reaped();
    }

    #[test]
    fn every_agent_serializes_the_capability_flags() {
        let keys = [
            "modes",
            "skills",
            "slashCommands",
            "compaction",
            "backgroundShells",
            "attachments",
            "stagedRevert",
            "attachmentsByPath",
        ];
        let flags = |f: crate::discovery::AgentFeatures| {
            let v = serde_json::to_value(f).unwrap();
            keys.map(|k| v[k].as_bool().unwrap_or_else(|| panic!("{k} missing")))
        };
        assert_eq!(
            flags(opencode_features(true, true)),
            [true, true, true, true, true, true, true, false]
        );
        assert_eq!(
            flags(deepseek_features(true, true)),
            [true, false, false, false, false, true, false, true]
        );
        for (features, expected) in [
            (opencode_features(false, false), [true, true, false]),
            (deepseek_features(false, false), [true, false, true]),
            (t3::t3_features(), [true, true, false]),
            (codex::features(), [true, true, true]),
        ] {
            assert_eq!(
                ["sessionRename", "sessionDelete", "sessionFork"]
                    .map(|key| features.extra[key].as_bool().unwrap()),
                expected
            );
        }
        for (features, pagination) in [
            (opencode_features(false, false), true),
            (deepseek_features(false, false), false),
        ] {
            let wire = serde_json::to_value(features).unwrap();
            assert_eq!(wire["sessionPagination"], pagination);
            assert!(wire.get("session_pagination").is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_uses_the_service_cli_and_waits_for_its_exit() {
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "test \"$#\" -eq 2 && test \"$1\" = service && test \"$2\" = start",
            "opencode",
        ]);
        launch_service(command, None)
            .await
            .expect("service start arguments");

        let mut failing = tokio::process::Command::new("sh");
        failing.args(["-c", "exit 7", "opencode"]);
        let err = launch_service(failing, None)
            .await
            .expect_err("failed startup");
        assert!(err.to_string().contains("exited with"), "{err}");
    }

    #[test]
    fn a_sibling_command_runs_with_the_same_environment() {
        let mut like = tokio::process::Command::new("opencode");
        like.env("XDG_CONFIG_HOME", "/x")
            .env_remove("OPENCODE_CONFIG");
        let copy = sibling_command(&like);
        assert_eq!(copy.as_std().get_program(), "opencode");
        let envs: Vec<_> = copy.as_std().get_envs().collect();
        assert_eq!(envs.len(), 2);
        assert!(envs.contains(&(
            std::ffi::OsStr::new("XDG_CONFIG_HOME"),
            Some(std::ffi::OsStr::new("/x"))
        )));
        assert!(envs.contains(&(std::ffi::OsStr::new("OPENCODE_CONFIG"), None)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_mcp_token_replaces_the_old_only_once_the_start_worked() {
        let state = crate::test_state("admin", Vec::new());
        let tokens = state.mcp_tokens.as_ref();
        const URL: &str = "http://127.0.0.1:23999/mcp";
        let (first, none) = issue_agent_env(tokens, URL).await.unwrap();
        assert!(none.is_empty());
        assert_eq!(first.url, URL);
        // An agent token does not expire; the next start replaces it.
        let rows = tokens.list().await.unwrap();
        assert_eq!(rows[0].expires_ms, None);

        // A failed start revokes the new token and leaves the old one.
        let (second, previous) = issue_agent_env(tokens, URL).await.unwrap();
        assert_eq!(previous, std::slice::from_ref(&first.token_id));
        settle_agent_tokens(tokens, &second, &previous, false).await;
        assert!(tokens.resolve(&first.token).await.unwrap().live().is_some());
        assert!(tokens
            .resolve(&second.token)
            .await
            .unwrap()
            .live()
            .is_none());

        // A good start retires the earlier tokens.
        let (third, previous) = issue_agent_env(tokens, URL).await.unwrap();
        assert_eq!(previous, std::slice::from_ref(&first.token_id));
        settle_agent_tokens(tokens, &third, &previous, true).await;
        assert!(tokens.resolve(&first.token).await.unwrap().live().is_none());
        assert!(tokens.resolve(&third.token).await.unwrap().live().is_some());

        // `launch_service` also passes both variables in the process env
        // (sh exits 0 only if it sees them). `sh` has no `service set`, so
        // those steps fail and are skipped.
        let mut command = tokio::process::Command::new("sh");
        command.args([
            "-c",
            "test \"$MUQUN_MCP_TOKEN\" = \"$1\" && test \"$MUQUN_MCP_URL\" = \"$2\"",
            "opencode",
            &third.token,
            &third.url,
        ]);
        launch_service(command, Some(&third))
            .await
            .expect("env reaches the child");
    }

    #[tokio::test]
    async fn startup_reports_a_missing_executable() {
        let missing =
            std::env::temp_dir().join(format!("missing-opencode-{}", uuid::Uuid::new_v4()));
        let err = launch_service(tokio::process::Command::new(missing), None)
            .await
            .expect_err("missing binary");
        assert!(
            err.to_string()
                .contains("could not run `opencode service start`"),
            "{err}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "Requires installed OpenCode 2; starts and stops a private service"]
    async fn installed_service_loads_saved_env_and_reuses_the_background_process() {
        struct PrivateService {
            root: std::path::PathBuf,
            binary: std::path::PathBuf,
        }
        impl PrivateService {
            fn command(&self) -> std::process::Command {
                let mut command = std::process::Command::new(&self.binary);
                for (key, path) in [
                    ("XDG_CONFIG_HOME", "config"),
                    ("XDG_STATE_HOME", "state"),
                    ("XDG_DATA_HOME", "data"),
                    ("XDG_CACHE_HOME", "cache"),
                    ("OPENCODE_DB", "opencode.db"),
                ] {
                    command.env(key, self.root.join(path));
                }
                for key in [
                    "OPENCODE_CONFIG",
                    "OPENCODE_CONFIG_CONTENT",
                    "OPENCODE_SERVER_PASSWORD",
                    "MUQUN_STARTUP_TEST",
                ] {
                    command.env_remove(key);
                }
                command.current_dir(&self.root);
                command
            }
        }
        impl Drop for PrivateService {
            fn drop(&mut self) {
                // Stop only the service registered under this fixture's XDG paths.
                let mut command = self.command();
                command
                    .args(["service", "stop"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                if command.status().is_ok_and(|status| status.success())
                    && !std::thread::panicking()
                {
                    let _ = std::fs::remove_dir_all(&self.root);
                } else {
                    eprintln!(
                        "Private OpenCode test files retained at {}",
                        self.root.display()
                    );
                }
            }
        }
        let service = PrivateService {
            root: std::env::temp_dir()
                .join(format!("muqun-opencode-start-{}", uuid::Uuid::new_v4())),
            binary: resolve_binary(None).expect("installed OpenCode"),
        };
        let config = service.root.join("config/opencode");
        std::fs::create_dir_all(&config).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        std::fs::write(
            config.join("service.json"),
            serde_json::to_vec(&serde_json::json!({
                "hostname": "127.0.0.1", "port": port,
                "env": { "MUQUN_STARTUP_TEST": "saved-service-env" }
            }))
            .unwrap(),
        )
        .unwrap();

        let mcp = McpEnv {
            token_id: "t".into(),
            token: "private-test-token".into(),
            url: "http://127.0.0.1:1/mcp".into(),
        };
        launch_service(service.command().into(), Some(&mcp))
            .await
            .expect("start private service");
        let registration_path = service.root.join("state/opencode/service.json");
        let first: super::super::adapters::opencode::OpencodeServiceRegistration =
            serde_json::from_slice(&std::fs::read(&registration_path).unwrap()).unwrap();
        let pid = first.pid.expect("background daemon PID");
        let environment = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        assert!(environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"MUQUN_STARTUP_TEST=saved-service-env"));
        assert!(environment
            .split(|byte| *byte == 0)
            .any(|entry| entry == b"MUQUN_MCP_TOKEN=private-test-token"));
        let endpoint = OpencodeEndpoint {
            url: first.url.clone(),
            password: first.password,
            version: first.version,
            pid: Some(pid),
        };
        assert!(endpoint.probe_healthy(&probe_client()).await);

        launch_service(service.command().into(), None)
            .await
            .expect("reuse private service");
        let second: super::super::adapters::opencode::OpencodeServiceRegistration =
            serde_json::from_slice(&std::fs::read(registration_path).unwrap()).unwrap();
        assert_eq!(second.pid, Some(pid));
        assert_eq!(second.url, first.url);
    }

    #[test]
    fn autostart_defaults_to_on_and_the_binary_is_optional() {
        let config: OpencodeConfig = serde_json::from_str("{}").expect("empty config parses");
        assert!(config.enabled);
        assert!(config.autostart);
        assert!(config.binary.is_none());
    }

    #[test]
    fn autostart_can_be_turned_off() {
        let config: OpencodeConfig =
            serde_json::from_str(r#"{"enabled":false,"autostart":false,"binary":"/opt/opencode"}"#)
                .expect("config parses");
        assert!(!config.enabled);
        assert!(!config.autostart);
        assert_eq!(config.binary.as_deref(), Some("/opt/opencode"));
    }

    /// The version gate is about the major number, and the string it reads
    /// comes from three different places in three different shapes.
    #[test]
    fn a_version_is_read_out_of_whatever_shape_it_arrives_in() {
        assert_eq!(parse_major("opencode v2.0.1"), Some(2));
        assert_eq!(parse_major("2.0.1"), Some(2));
        assert_eq!(parse_major("v2.0.1-beta.3"), Some(2));
        assert_eq!(parse_major("opencode 1.18.4"), Some(1));
        assert_eq!(parse_major("v10.2.0"), Some(10));
        assert_eq!(parse_major("2"), Some(2));
        assert_eq!(parse_major("not a version"), None);
        assert_eq!(parse_major(""), None);
    }

    /// v1 is a different API. Attaching to it looked like success and then
    /// failed on every route, which is a worse answer than refusing.
    #[test]
    fn a_v1_agent_is_refused_and_the_refusal_says_what_to_do() {
        let refusal = check_version(Some("opencode 1.18.4")).expect_err("v1 is refused");
        assert!(
            refusal.contains("2.x only"),
            "it names the supported range: {refusal}"
        );
        assert!(
            refusal.contains("opencode.binary"),
            "and how to point it elsewhere: {refusal}"
        );

        assert!(check_version(Some("opencode v2.0.1")).is_ok());
        assert!(check_version(Some("v3.0.0")).is_err());
        assert!(check_version(Some("2.0.27")).is_ok());
        assert!(check_version(Some("2.0.99")).is_ok());
    }

    /// Silence is not evidence of being old. An install that reports no
    /// version, or one this cannot parse, is allowed through rather than
    /// refused on a guess.
    #[test]
    fn an_unreadable_version_is_not_treated_as_too_old() {
        assert!(check_version(None).is_ok());
        assert!(check_version(Some("")).is_ok());
        assert!(check_version(Some("   ")).is_ok());
        assert!(check_version(Some("unknown build")).is_ok());
    }

    /// `opencode.binary` first, then `opencode` as PATH resolves it. No
    /// install directory is guessed at, so this is the whole order.
    /// `opencode.binary` first, then `opencode` as PATH resolves it. No
    /// install directory is guessed at, so this is the whole order.
    #[test]
    fn the_binary_is_the_configured_one_or_whatever_path_says() {
        let dir =
            std::env::temp_dir().join(format!("muqun-resolve-{}", uuid::Uuid::new_v4().simple()));
        let other = dir.join("elsewhere");
        std::fs::create_dir_all(&other).expect("temp dir");

        let on_path = dir.join("opencode");
        let configured = other.join("opencode-2");
        for file in [&on_path, &configured] {
            std::fs::write(file, "#!/bin/sh\nexit 0\n").expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        let path_var = std::ffi::OsString::from(&dir);
        let empty_path = std::ffi::OsString::from(&other);

        // An absolute `opencode.binary` is taken literally, whatever PATH says.
        assert_eq!(
            resolve_binary_in(Some(configured.to_str().unwrap()), Some(&path_var))
                .expect("configured"),
            std::fs::canonicalize(&configured).unwrap()
        );

        // A configured path that is not there names itself rather than
        // silently falling back to PATH.
        let missing = other.join("not-here");
        let err = resolve_binary_in(Some(missing.to_str().unwrap()), Some(&path_var))
            .expect_err("missing");
        assert!(err.to_string().contains("not-here"), "got {err}");

        // Absent, it is `opencode` on PATH -- and the answer is the file.
        assert_eq!(
            resolve_binary_in(None, Some(&path_var)).expect("found on PATH"),
            std::fs::canonicalize(&on_path).unwrap()
        );
        // A bare name is looked up the same way.
        assert_eq!(
            resolve_binary_in(Some("opencode"), Some(&path_var)).expect("bare name"),
            std::fs::canonicalize(&on_path).unwrap()
        );

        // Nothing named `opencode` anywhere on PATH: an error that says how to
        // fix it, not a guess at an install directory.
        let err = resolve_binary_in(None, Some(&empty_path)).expect_err("not there");
        assert!(err.to_string().contains("not on PATH"), "got {err}");
        assert!(err.to_string().contains("opencode.binary"), "got {err}");

        let err = resolve_binary_in(None, None).expect_err("no PATH at all");
        assert!(err.to_string().contains("PATH is not set"), "got {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installation_status_distinguishes_local_lookup_from_service_readiness() {
        let dir = std::env::temp_dir().join(format!(
            "muqun-installation-status-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let binary = dir.join("opencode");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").expect("write binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let path_var = std::ffi::OsString::from(&dir);
        let empty_path = std::ffi::OsString::from(dir.join("empty"));
        let missing = dir.join("missing-opencode");
        let non_executable = dir.join("not-executable");
        std::fs::write(&non_executable, "not a program\n").expect("write non-executable");

        assert_eq!(
            installation_status_in(None, Some(&path_var), false),
            AgentInstallation::Installed,
            "a local executable is installed even while its service is down"
        );
        assert_eq!(
            installation_status_in(Some(missing.to_str().unwrap()), Some(&path_var), false),
            AgentInstallation::NotFound,
            "a broken explicit path does not fall back to PATH"
        );
        #[cfg(unix)]
        assert_eq!(
            installation_status_in(
                Some(non_executable.to_str().unwrap()),
                Some(&path_var),
                false
            ),
            AgentInstallation::NotFound,
            "an explicit regular file must also be executable"
        );
        assert_eq!(
            installation_status_in(None, Some(&empty_path), false),
            AgentInstallation::NotFound
        );
        assert_eq!(
            installation_status_in(None, None, false),
            AgentInstallation::Unknown,
            "without PATH the local lookup is inconclusive"
        );
        assert_eq!(
            installation_status_in(None, Some(&path_var), true),
            AgentInstallation::Unknown,
            "the local PATH cannot establish installation for an external endpoint"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoint_environment_errors_make_installation_inconclusive() {
        let unreadable = std::ffi::OsString::from("unreadable");
        assert_eq!(
            external_endpoint_configured_with(|name| match name {
                "OPENCODE_URL" => Err(std::env::VarError::NotUnicode(unreadable.clone())),
                _ => Err(std::env::VarError::NotPresent),
            }),
            Err(())
        );
        assert_eq!(
            external_endpoint_configured_with(|name| match name {
                "OPENCODE_URL" => Err(std::env::VarError::NotPresent),
                "HERDR_GATEWAY_OPENCODE_URL" => Ok(" http://127.0.0.1:4096 ".to_string()),
                _ => unreachable!(),
            }),
            Ok(true)
        );
    }

    #[test]
    fn agent_status_serializes_the_additive_installation_field() {
        let status = AgentStatus {
            available: false,
            installation: AgentInstallation::NotFound,
            origin: AgentOrigin::None,
            url: None,
            version: None,
            stream_connected: false,
            autostart: true,
            kind: None,
            agent_id: None,
        };
        let value = serde_json::to_value(status).expect("status serializes");
        assert_eq!(value["installation"], "not_found");
        assert_eq!(value["available"], false);
        assert_eq!(value["origin"], "none");
        assert_eq!(value["autostart"], true);
        assert!(value.get("url").is_none());
        assert!(value.get("version").is_none());
    }

    /// A lost stream is the agent going away, so the first one is looked at
    /// at once; a stream that keeps dropping must not spin the supervisor.
    #[test]
    fn a_flapping_stream_backs_off_but_the_first_loss_does_not_wait() {
        assert_eq!(STREAM_LOSS_FIRST_WAIT, Duration::ZERO);
        let mut wait = STREAM_LOSS_FIRST_WAIT;
        wait = next_stream_loss_wait(wait);
        assert_eq!(wait, Duration::from_secs(1));
        let mut seen = vec![wait];
        for _ in 0..8 {
            wait = next_stream_loss_wait(wait);
            seen.push(wait);
        }
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "it only grows: {seen:?}"
        );
        assert_eq!(*seen.last().unwrap(), STREAM_LOSS_MAX_WAIT, "and it stops");
        assert!(
            STREAM_WATCH_TICK < HEALTHY_POLL,
            "the stream is watched more often than the agent is polled"
        );
    }

    #[tokio::test]
    async fn a_disabled_runtime_attaches_nothing_and_never_spawns() {
        let runtime = AgentRuntime::disabled();
        assert!(runtime.manager().await.is_none());
        let status = runtime.status().await;
        assert!(!status.available);
        assert!(!status.autostart);
        assert_eq!(status.origin, AgentOrigin::None);
        // A subscriber works with no agent attached, which is what lets a
        // stream outlive a reconnect.
        let _rx = runtime.subscribe_events();
    }

    #[tokio::test]
    async fn closed_codex_is_not_available_or_advertised_from_cached_discovery() {
        use crate::agents::adapters::codex::tests::Fixture;
        let mut fixture = Fixture::new(|_| serde_json::json!({})).await;
        fixture.client.initialize().await.unwrap();
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig::default(),
            None,
        );
        runtime
            .install(
                "codex",
                AgentManager::connect_codex(
                    fixture.client.clone(),
                    fixture.events.take().unwrap(),
                    runtime.events_tx.clone(),
                ),
                AgentOrigin::Adopted,
            )
            .await;
        let found = runtime.attached_snapshot().await;
        assert_eq!(
            found
                .agents
                .iter()
                .find(|a| a.id == "codex")
                .unwrap()
                .status,
            crate::discovery::AgentAvailability::Connected
        );
        *runtime.discovery_cache.write().await = Some((tokio::time::Instant::now(), found));
        fixture.client.stop();
        // Keep the mirror accessible for retained history; mutating calls are
        // fenced by the driver's exact client instance before writing any RPC.
        assert!(runtime.manager_for_agent("codex").await.is_some());
        assert!(!runtime.status_for("codex").await.unwrap().available);
        let found = runtime.discover_agents().await;
        let codex = found.agents.iter().find(|a| a.id == "codex").unwrap();
        assert_eq!(codex.status, crate::discovery::AgentAvailability::Offline);
        assert_eq!(
            codex.features.extra.get("sharedDaemonConnected"),
            Some(&serde_json::json!(false))
        );
    }

    #[tokio::test]
    async fn multi_agent_session_routing_and_all_managers() {
        let runtime = AgentRuntime::disabled();
        assert_eq!(runtime.all_managers().await.len(), 0);
        assert!(runtime.manager_for_agent("deepseek").await.is_none());
        assert!(runtime.manager_for_agent("opencode").await.is_none());
        assert!(runtime.manager_for_session("ses_123").await.is_none());

        runtime.record_session_route("ses_123", "deepseek").await;
        let routes = runtime.session_routes.read().await;
        assert_eq!(routes.map.get("ses_123"), Some(&"deepseek".to_string()));
    }

    #[test]
    fn primary_follows_the_preference_order_not_the_map_order() {
        let mut map = BTreeMap::new();
        assert_eq!(pick_primary(&map), None);
        map.insert("deepseek".to_string(), 2);
        assert_eq!(pick_primary(&map), Some(&2));
        map.insert("opencode".to_string(), 1);
        assert_eq!(
            pick_primary(&map),
            Some(&1),
            "OpenCode wins when both are attached"
        );
        map.insert("codex".to_string(), 5);
        assert_eq!(
            pick_primary(&map),
            Some(&5),
            "Codex wins among attached providers"
        );
        map.remove("codex");
        assert_eq!(
            pick_primary(&map),
            Some(&1),
            "OpenCode remains the fallback"
        );
        map.insert("other".to_string(), 3);
        map.remove("opencode");
        assert_eq!(pick_primary(&map), Some(&2));
        map.insert("t3".to_string(), 4);
        assert_eq!(pick_primary(&map), Some(&2), "DeepSeek outranks T3");
        map.remove("deepseek");
        assert_eq!(pick_primary(&map), Some(&4));
    }

    #[test]
    fn session_routes_are_bounded_and_evictable() {
        let mut routes = SessionRoutes::default();
        for i in 0..MAX_SESSION_ROUTES + 10 {
            routes.insert(&format!("s{i}"), "opencode");
        }
        assert_eq!(routes.map.len(), MAX_SESSION_ROUTES);
        assert!(!routes.map.contains_key("s0"), "the oldest went first");
        assert!(routes
            .map
            .contains_key(&format!("s{}", MAX_SESSION_ROUTES + 9)));
        routes.insert("s20", "deepseek");
        assert_eq!(
            routes.order.len(),
            MAX_SESSION_ROUTES,
            "re-recording adds no entry"
        );
        routes.remove("s20");
        assert!(!routes.map.contains_key("s20"));
        assert_eq!(routes.order.len(), MAX_SESSION_ROUTES - 1);
    }

    #[tokio::test]
    async fn an_unrecorded_session_with_nothing_attached_is_owned_by_nobody() {
        let runtime = AgentRuntime::disabled();
        assert!(runtime.manager_for_session("ses_unknown").await.is_none());
        runtime.record_session_route("ses_x", "opencode").await;
        runtime.forget_session_route("ses_x").await;
        assert!(!runtime
            .session_routes
            .read()
            .await
            .map
            .contains_key("ses_x"));
    }

    #[test]
    fn only_the_preference_list_names_known_agents() {
        let runtime = AgentRuntime::disabled();
        assert!(runtime.is_known_agent("opencode"));
        assert!(runtime.is_known_agent("deepseek"));
        assert!(runtime.is_known_agent("t3"));
        assert!(!runtime.is_known_agent("claude"));
    }

    #[tokio::test]
    async fn a_disabled_deepseek_is_not_probed_or_attached() {
        let runtime = AgentRuntime::disabled();
        let mut backoff = Duration::from_secs(2);
        assert!(!runtime.check_and_repair(&mut backoff).await);
        assert!(runtime.manager_for_agent("deepseek").await.is_none());
        let found = runtime.discover_agents().await;
        let ds = found.agents.iter().find(|a| a.id == "deepseek").unwrap();
        assert_eq!(ds.status, crate::discovery::AgentAvailability::Disabled);
        assert!(ds.endpoint.is_none());
    }

    fn t3_runtime(t3: T3Config) -> Arc<AgentRuntime> {
        AgentRuntime::with_t3_state(
            OpencodeConfig {
                enabled: false,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3State::with_env(t3, None, |_| None),
        )
    }

    #[tokio::test]
    async fn a_disabled_t3_is_never_probed_attached_or_scanned_for() {
        let runtime = t3_runtime(T3Config::default());
        assert!(!runtime.supervise_t3().await);
        assert!(runtime.manager_for_agent("t3").await.is_none());
        let found = runtime.discover_agents().await;
        let t3 = found.agents.iter().find(|a| a.id == "t3").unwrap();
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Disabled);
        assert!(!t3.enabled);
        assert!(t3.endpoint.is_none(), "no default URL is advertised");
        assert!(runtime.status_for("t3").await.is_none());
    }

    #[tokio::test]
    async fn an_enabled_t3_without_a_server_is_offline_at_its_url() {
        // Port 9 (discard) refuses on loopback: nothing answers there.
        let runtime = t3_runtime(T3Config {
            url: Some("http://127.0.0.1:9/".into()),
            ..Default::default()
        });
        assert!(!runtime.supervise_t3().await);
        assert!(runtime.manager_for_agent("t3").await.is_none());
        let found = runtime.discover_agents().await;
        let t3 = found.agents.iter().find(|a| a.id == "t3").unwrap();
        assert_eq!(t3.name, "T3 Code");
        assert_eq!(t3.kind, "t3");
        assert!(t3.enabled);
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Offline);
        assert_eq!(t3.endpoint.as_deref(), Some("http://127.0.0.1:9/"));
        assert!(t3.modes.is_empty());
        let f = &t3.features;
        assert!(f.streaming && f.model_selection && f.tool_approvals);
        assert!(!f.revert && !f.reasoning_effort && !f.worktrees && !f.inbox);

        let enabled = t3_runtime(T3Config {
            enabled: true,
            ..Default::default()
        });
        let found = enabled.attached_snapshot().await;
        let t3 = found.agents.iter().find(|a| a.id == "t3").unwrap();
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Offline);
        assert_eq!(enabled.t3.url(), t3::DEFAULT_T3_URL);
    }

    /// A T3 server that answers only its public descriptor.
    async fn fake_t3_descriptor() -> String {
        fake_t3_serving(r#"{"environmentId":"env","label":"t3","serverVersion":"0.9.1"}"#).await
    }

    /// A T3 server that answers `descriptor` at its well-known path.
    async fn fake_t3_serving(descriptor: &'static str) -> String {
        let app = axum::Router::new().route(
            crate::agents::adapters::t3::endpoint::WELL_KNOWN_PATH,
            axum::routing::get(move || async move { descriptor }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        url
    }

    #[tokio::test]
    async fn a_t3_server_without_a_credential_is_unconfigured_not_reachable() {
        let url = fake_t3_descriptor().await;
        let runtime = t3_runtime(T3Config {
            url: Some(url.clone()),
            ..Default::default()
        });
        let t3 = runtime.t3_discovery().await;
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Unconfigured);
        assert_eq!(t3.version.as_deref(), Some("0.9.1"));

        // Holding a credential, the gateway will attach on first use.
        let runtime = t3_runtime(T3Config {
            url: Some(url),
            token: Some("bearer".into()),
            ..Default::default()
        });
        let t3 = runtime.t3_discovery().await;
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Reachable);
    }

    /// The live 0.0.45 descriptor, with the protocol version swapped in.
    fn t3_descriptor_speaking(version: u64) -> &'static str {
        let mut raw: serde_json::Value =
            serde_json::from_str(include_str!("adapters/t3/fixtures/environment.json")).unwrap();
        raw["serverVersion"] = "0.0.45".into();
        raw["orchestrationProtocolVersion"] = version.into();
        Box::leak(raw.to_string().into_boxed_str())
    }

    #[tokio::test]
    async fn a_t3_server_on_a_newer_protocol_is_unsupported_and_never_attached() {
        use crate::agents::adapters::t3::endpoint::{TOKEN_PATH, WELL_KNOWN_PATH};
        use axum::{
            http::StatusCode,
            routing::{get, post},
            Router,
        };

        let exchanges = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = exchanges.clone();
        let descriptor = t3_descriptor_speaking(3);
        let app = Router::new()
            .route(WELL_KNOWN_PATH, get(move || async move { descriptor }))
            .route(
                TOKEN_PATH,
                post(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        StatusCode::INTERNAL_SERVER_ERROR
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for credential in [
            T3Config {
                token: Some("fixture-bearer".into()),
                ..Default::default()
            },
            T3Config {
                pairing_token: Some("fixture-pairing-token".into()),
                ..Default::default()
            },
        ] {
            let runtime = t3_runtime(T3Config {
                url: Some(url.clone()),
                ..credential
            });
            assert!(!runtime.supervise_t3().await);
            assert!(runtime.manager_for_agent("t3").await.is_none());
            let t3 = runtime.t3_discovery().await;
            assert_eq!(t3.status, crate::discovery::AgentAvailability::Unsupported);
            assert_eq!(
                t3.reason.as_deref(),
                Some("T3 server speaks orchestration protocol 3; this gateway supports 1 and 2")
            );
            assert_eq!(t3.version.as_deref(), Some("0.0.45"));
            assert!(t3.models.is_empty());
            let wire = serde_json::to_value(&t3).unwrap();
            assert_eq!(wire["status"], "unsupported");
            assert_eq!(
                exchanges.load(Ordering::SeqCst),
                0,
                "unsupported protocols must not consume a pairing token"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_t3_server_on_protocol_1_is_attached() {
        let url = fake_t3_serving(t3_descriptor_speaking(1)).await;
        let runtime = t3_runtime(T3Config {
            url: Some(url),
            token: Some("bearer".into()),
            ..Default::default()
        });
        assert!(runtime.supervise_t3().await);
        let manager = runtime.manager_for_agent("t3").await.expect("attached");
        assert_eq!(manager.version().as_deref(), Some("0.0.45"));
        let t3 = runtime.t3_discovery().await;
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Connected);
        assert!(t3.reason.is_none());
        assert!(
            t3.features.revert,
            "known protocol 1 supports one-step revert"
        );
        let snapshot = runtime.attached_snapshot().await;
        assert!(
            snapshot
                .agents
                .iter()
                .find(|a| a.id == "t3")
                .unwrap()
                .features
                .revert
        );
        manager.shutdown();
    }

    #[tokio::test]
    async fn a_t3_server_on_protocol_2_is_attached_and_discovered_with_its_server_version() {
        // Select the protocol from the descriptor, not the serverVersion string.
        let url = fake_t3_serving(t3_descriptor_speaking(2)).await;
        let runtime = t3_runtime(T3Config {
            url: Some(url.clone()),
            token: Some("fixture-bearer".into()),
            ..Default::default()
        });
        let reachable = runtime.t3_discovery().await;
        assert_eq!(
            reachable.status,
            crate::discovery::AgentAvailability::Reachable
        );
        assert_eq!(reachable.version.as_deref(), Some("0.0.45"));
        assert!(reachable.reason.is_none());
        assert!(!reachable.features.revert);

        assert!(runtime.supervise_t3().await);
        let manager = runtime
            .manager_for_agent("t3")
            .await
            .expect("protocol 2 attached");
        assert_eq!(manager.version().as_deref(), Some("0.0.45"));
        let connected = runtime.t3_discovery().await;
        assert_eq!(
            connected.status,
            crate::discovery::AgentAvailability::Connected
        );
        assert_eq!(connected.endpoint.as_deref(), Some(url.as_str()));
        assert_eq!(connected.version.as_deref(), Some("0.0.45"));
        assert!(connected.reason.is_none());
        assert!(!connected.features.revert);
        let snapshot = runtime.attached_snapshot().await;
        assert!(
            !snapshot
                .agents
                .iter()
                .find(|a| a.id == "t3")
                .unwrap()
                .features
                .revert
        );
        let wire = serde_json::to_value(&connected.features).unwrap();
        assert_eq!(wire["orchestrationProtocols"], serde_json::json!([1, 2]));
        assert_eq!(wire["sessionPagination"], false);
        assert_eq!(
            runtime.status_for("t3").await.unwrap().version.as_deref(),
            Some("0.0.45")
        );
        manager.shutdown();
    }

    #[tokio::test]
    async fn an_attached_t3_is_connected_with_its_server_version() {
        let runtime = t3_runtime(T3Config {
            enabled: true,
            ..Default::default()
        });
        let driver = Arc::new(crate::agents::adapters::t3::T3Driver::new(
            crate::agents::adapters::t3::T3Endpoint::new(
                "http://127.0.0.1:9",
                crate::agents::adapters::t3::T3Credential::None,
            ),
        ));
        let client = driver.client().clone();
        runtime
            .attach_for_test(Arc::new(AgentManager::for_test(driver)))
            .await;
        let found = runtime.discover_agents().await;
        let t3 = found.agents.iter().find(|a| a.id == "t3").unwrap();
        assert_eq!(t3.status, crate::discovery::AgentAvailability::Connected);
        assert!(found.supported);
        let status = runtime.status_for("t3").await.expect("attached");
        assert_eq!(status.agent_id.as_deref(), Some("t3"));
        client.shutdown();
    }

    #[tokio::test]
    async fn runtime_discovery_orders_all_appended_connected_providers_and_preserves_routes() {
        let runtime = AgentRuntime::with_all_configs(
            OpencodeConfig {
                enabled: true,
                autostart: false,
                binary: None,
            },
            DeepseekConfig::default(),
            T3Config::default(),
            CodexConfig::default(),
            None,
        );
        runtime
            .attach_for_test(Arc::new(AgentManager::for_test(Arc::new(
                crate::test_support::FakeAgent::new("opencode"),
            ))))
            .await;
        let mut fixture = crate::agents::adapters::codex::tests::Fixture::new(|request| {
            match request["method"].as_str().unwrap() {
                "config/read" => serde_json::json!({"config":{"model_provider":"openai"}}),
                "model/list" => {
                    crate::agents::adapters::codex::tests::golden()["modelList"].clone()
                }
                // Catalog reads skills/list after models. An empty list is a
                // real answer; it must not be treated as an unexpected mutation.
                "skills/list" => serde_json::json!({"data": []}),
                other => panic!("unexpected discovery mutation {other}"),
            }
        })
        .await;
        fixture.ready().await;
        runtime
            .attach_for_test(Arc::new(AgentManager::connect_codex(
                fixture.client.clone(),
                fixture.events.take().unwrap(),
                runtime.events_tx.clone(),
            )))
            .await;
        runtime
            .record_session_route("synthetic-session", "opencode")
            .await;
        let found = runtime.probe_agents().await;
        assert_eq!(
            found
                .agents
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["codex", "opencode", "deepseek", "t3"]
        );
        assert_eq!(runtime.manager().await.unwrap().agent().kind(), "codex");
        assert_eq!(
            runtime
                .manager_for_session("synthetic-session")
                .await
                .unwrap()
                .agent()
                .kind(),
            "opencode"
        );
        let fallback = runtime.attached_snapshot().await;
        assert_eq!(fallback.agents[0].id, "codex");
        assert!(
            !fallback.agents[0].features.model_selection,
            "an unqueried catalog is unknown"
        );
    }

    #[tokio::test]
    async fn discovery_lists_agents_in_preference_order() {
        let runtime = AgentRuntime::disabled();
        let found = runtime.discover_agents().await;
        let ids: Vec<&str> = found.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["opencode", "deepseek", "t3", "codex"]);
    }

    #[tokio::test]
    async fn agent_discovery_is_memoized() {
        let runtime = AgentRuntime::disabled();
        let first = runtime.discover_agents().await;
        assert!(runtime.discovery_cache.read().await.is_some());
        // Poison the cache; a second call inside the TTL must return it as is.
        runtime
            .discovery_cache
            .write()
            .await
            .as_mut()
            .unwrap()
            .1
            .supported = true;
        let second = runtime.discover_agents().await;
        assert!(second.supported, "the poisoned cache answered");
        assert!(
            !first.supported,
            "nothing is attached to a disabled runtime"
        );
    }
}
