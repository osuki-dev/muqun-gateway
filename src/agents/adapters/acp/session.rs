use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use super::client::AcpRetirement;
use super::mapper::{self, Permission, Projection};
use super::operations::{self, PromptCapabilities};
use super::recovery::{self, Checkpoint, NativeSnapshot, TurnEvidence};
use super::{AcpClient, AcpEvent};
use crate::agents::adapters::memory_mirror::MemoryMirror;
use crate::agents::domain::*;
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};
use crate::agents::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};
use std::marker::PhantomData;

/// Native launch and verified differences; protocol lifecycle is shared.
pub trait AcpProvider: Send + Sync + 'static {
    const KIND: &'static str;
    const LABEL: &'static str;
    const ARGS: &'static [&'static str];
    const REQUIRE_MESSAGE_ID: bool;
    const MODEL_SELECTION: bool;
    const SESSION_NOTICES: bool = false;
    const IDLE_RETIREMENT: bool = false;
    /// How long `initialize` may take; a runner that downloads the agent on
    /// first use needs longer than a normal request.
    const INITIALIZE_TIMEOUT: Duration = RPC_TIMEOUT;
    fn recovery_snapshot<'a>(
        _binary: &'a str,
        _directory: &'a str,
        _id: &'a str,
        _stop: &'a CancellationToken,
    ) -> AgentFuture<'a, NativeSnapshot> {
        unsupported("read-only native recovery evidence")
    }
    const PROVIDER_CONFIG: bool = true;
    const EFFORT_CONFIG: &'static str = "effort";
    fn session_metadata<'a>(
        rows: Vec<Value>,
        _stop: &'a CancellationToken,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async move { Ok(rows) })
    }
    fn global_catalog<'a>(
        _binary: &'a str,
        _directory: &'a str,
        _stop: &'a CancellationToken,
    ) -> AgentFuture<'a, AgentCatalog> {
        unsupported("global ACP model catalog")
    }
    /// How long a successful global catalog is reused. Discovery asks on
    /// every refresh, and each lookup launches a native process.
    const CATALOG_TTL: Duration = Duration::from_secs(60);
    /// Cheap token that changes whenever the native saved-session store does.
    /// While it holds, a listing stays valid for `SESSION_STORE_MAX_AGE`, so
    /// periodic Home refreshes do not launch a process to learn nothing new.
    fn session_store_fingerprint() -> Option<u64> {
        None
    }
    fn account_limits<'a>(
        _binary: &'a str,
        _directory: &'a str,
        _stop: &'a CancellationToken,
    ) -> AgentFuture<'a, crate::agents::domain::account_limits::AccountLimits> {
        unsupported("ACP account limits")
    }
    fn safe_error_message(_: &Value) -> Option<&'static str> {
        None
    }
    /// History of a session another process (a terminal) is writing, read
    /// from the native store without loading it. See [`TerminalBridge`].
    fn live_timeline<'a>(
        _directory: &'a str,
        _id: &'a str,
        _stop: &'a CancellationToken,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        unsupported("reading a session held by a terminal")
    }
    /// The model and effort the native store says the session last ran
    /// with, applied when it is loaded so it does not fall back to the
    /// agent's default. `None` when the store records none.
    fn transcript_model<'a>(
        _directory: &'a str,
        _id: &'a str,
    ) -> AgentFuture<'a, Option<(String, Option<String>)>> {
        Box::pin(async { Ok(None) })
    }
    /// When that store last changed for `id`, to notice terminal progress.
    fn transcript_modified(_directory: &str, _id: &str) -> Option<std::time::SystemTime> {
        None
    }
    /// Rows for `session/list` read from the native store without starting
    /// the agent, or `None` to ask the agent itself.
    fn offline_sessions<'a>(
        _directory: Option<&'a str>,
        _stop: &'a CancellationToken,
    ) -> Option<AgentFuture<'a, Vec<Value>>> {
        None
    }
}

/// A session that another process on this computer (a terminal) has open.
/// The driver never starts a second agent process for it: history is read
/// from the native store and input goes into that terminal.
#[derive(Clone, Debug)]
pub struct TerminalLive {
    pub runtime: SessionRuntime,
    /// Where typed input goes; `None` when no terminal pane holds the process.
    pub route: Option<TerminalRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRoute {
    /// The Gateway terminal session (tmux/Herdr) and pane.
    pub session: String,
    pub pane: String,
}

/// Correlates sessions with terminal processes and reaches their panes.
pub trait TerminalBridge: Send + Sync {
    fn live<'a>(&'a self, ids: &'a [String]) -> AgentFuture<'a, HashMap<String, TerminalLive>>;
    fn send<'a>(&'a self, route: &'a TerminalRoute, text: &'a str) -> AgentFuture<'a, ()>;
    fn interrupt<'a>(&'a self, route: &'a TerminalRoute) -> AgentFuture<'a, ()>;
}

/// How long a terminal-held transcript is followed after input from here.
const TERMINAL_FOLLOW: Duration = Duration::from_secs(600);

/// How the driver starts its ACP child when it is not `binary` with
/// [`AcpProvider::ARGS`]: a runner's arguments and the child's environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpLaunch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;
const RETIREMENT_TIMEOUT: Duration = Duration::from_secs(15);
// At most 8 MiB of serialized dormant info, plus bounded map/index overhead.
const MAX_DORMANT_SESSIONS: usize = 512;
const MAX_DORMANT_INFO_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 16;
const MAX_LIST_PAGES: usize = 64;
const SESSION_METADATA_TTL: Duration = Duration::from_secs(30);
const SESSION_LIST_TTL: Duration = SESSION_METADATA_TTL;
const SESSION_STORE_MAX_AGE: Duration = Duration::from_secs(600);
const MAX_LIST_SCOPES: usize = 4;
const MAX_CACHED_LIST_ROWS: usize = 4096;

struct CachedSessionList {
    directory: Option<String>,
    listed_at: tokio::time::Instant,
    fingerprint: Option<u64>,
    sessions: Vec<AgentSessionInfo>,
}

impl CachedSessionList {
    fn fresh(&self, fingerprint: Option<u64>) -> bool {
        let age = self.listed_at.elapsed();
        age < SESSION_LIST_TTL
            || (fingerprint.is_some()
                && self.fingerprint == fingerprint
                && age < SESSION_STORE_MAX_AGE)
    }
}

struct SessionMetadata {
    listed_at: tokio::time::Instant,
    sessions: HashMap<String, AgentSessionInfo>,
}

struct SessionState {
    info: AgentSessionInfo,
    catalog: AgentCatalog,
    config: Value,
    owned_mutations: bool,
    usage: mapper::SessionUsage,
    projection: Projection,
    permissions: HashMap<String, Permission>,
    last_activity: tokio::time::Instant,
    recovery: Option<RecoveryEvidence>,
    prompt_checkpoint: Option<Checkpoint>,
    prompt_user_digest: Option<String>,
    turn_had_output: bool,
}

#[derive(Clone)]
enum RecoveryEvidence {
    Loaded(Checkpoint),
    Turn {
        before: Checkpoint,
        expected: TurnEvidence,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DormantKind {
    Parked,
    Unexpected,
}

#[derive(Clone)]
struct DormantSession {
    info: AgentSessionInfo,
    owned_mutations: bool,
    kind: DormantKind,
    complete_metadata: bool,
}

fn dormant_metadata_fits(info: &AgentSessionInfo) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_DORMANT_INFO_BYTES.saturating_sub(self.0) {
                return Err(std::io::Error::other("dormant metadata byte budget"));
            }
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), info).is_ok()
}

fn parked_runtime() -> SessionRuntime {
    SessionRuntime {
        transcript: Some(TranscriptSource::Snapshot),
        source: RuntimeSource::Native,
        interaction: RuntimeInteraction::Native,
        status: Some(TerminalRuntimeStatus::Idle),
        terminal: None,
        reason: Some(RuntimeReason::NotLoaded),
    }
}

fn parked_metadata_fits(info: &AgentSessionInfo) -> bool {
    if !dormant_metadata_fits(info) {
        return false;
    }
    let mut parked = info.clone();
    parked.runtime = Some(parked_runtime());
    dormant_metadata_fits(&parked)
}

impl DormantSession {
    fn new(info: &AgentSessionInfo, owned_mutations: bool, kind: DormantKind) -> Self {
        let complete_metadata = dormant_metadata_fits(info);
        let info = if complete_metadata {
            info.clone()
        } else {
            // Display reads retain the full title in the bounded mirror. Do
            // not duplicate an oversized native JSON tree in another store.
            let mut compact = mapper::session(
                &info.asid.0,
                info.directory.as_deref().unwrap_or_default(),
                &Value::Null,
            );
            compact.status = AgentSessionStatus::Unknown;
            compact.runtime = Some(SessionRuntime {
                transcript: Some(TranscriptSource::Snapshot),
                source: RuntimeSource::Native,
                interaction: RuntimeInteraction::Unavailable,
                status: Some(TerminalRuntimeStatus::Unknown),
                terminal: None,
                reason: Some(RuntimeReason::NotLoaded),
            });
            if !dormant_metadata_fits(&compact) {
                compact.directory = None;
            }
            compact
        };
        Self {
            info,
            owned_mutations,
            kind,
            complete_metadata,
        }
    }

    fn overlay(&self, info: &mut AgentSessionInfo) {
        if self.complete_metadata {
            *info = self.info.clone();
        } else {
            info.status = self.info.status;
            info.runtime = self.info.runtime.clone();
        }
    }
}

struct Session {
    client: Arc<AcpClient>,
    state: Mutex<SessionState>,
    busy: AtomicBool,
    connected: AtomicBool,
    completions: mpsc::UnboundedSender<Result<Value, String>>,
    native_capabilities: Value,
    commands_observed: Arc<AtomicBool>,
    prompt_capabilities: PromptCapabilities,
    finished: CancellationToken,
    retirement: Mutex<()>,
}

type LiveSessions = Arc<StdMutex<HashMap<String, Arc<Session>>>>;
type DormantSessions = Arc<StdMutex<HashMap<String, DormantSession>>>;

struct ListenerContext {
    sessions: LiveSessions,
    dormant: DormantSessions,
    idle_timeout_secs: Arc<AtomicU64>,
    binary: String,
    inspection: Arc<Mutex<()>>,
}

struct Admission<'a> {
    busy: &'a AtomicBool,
    handed_off: bool,
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        if !self.handed_off {
            self.busy.store(false, Ordering::Release);
        }
    }
}

type CatalogCacheEntry = (
    String,
    tokio::time::Instant,
    Result<AgentCatalog, AgentError>,
);

pub struct AcpDriver<P: AcpProvider> {
    provider: PhantomData<P>,
    binary: String,
    default_directory: String,
    mirror: Arc<MemoryMirror>,
    tx: broadcast::Sender<AgentDomainEvent>,
    sessions: LiveSessions,
    dormant: DormantSessions,
    idle_timeout_secs: Arc<AtomicU64>,
    inspection: Arc<Mutex<()>>,
    connections: StdMutex<Vec<Weak<AcpClient>>>,
    connection_admission: Mutex<()>,
    starting: tokio::sync::RwLock<()>,
    mutations: Mutex<()>,
    global_catalog: Mutex<Option<CatalogCacheEntry>>,
    session_metadata: Mutex<Option<SessionMetadata>>,
    metadata_refresh: Mutex<()>,
    session_lists: Mutex<Vec<CachedSessionList>>,
    session_store: StdMutex<fn() -> Option<u64>>,
    list_refreshes: StdMutex<HashMap<Option<String>, Weak<Mutex<()>>>>,
    metadata_revision: AtomicU64,
    native_capabilities: StdMutex<Value>,
    commands_observed: Arc<AtomicBool>,
    launch: StdMutex<Option<AcpLaunch>>,
    mcp_servers: StdMutex<Value>,
    terminal: StdMutex<Option<Arc<dyn TerminalBridge>>>,
    me: Weak<Self>,
    /// The last model catalog a live session offered, kept after its
    /// process is released so choices stay available.
    last_catalog: StdMutex<Option<AgentCatalog>>,
    stop: CancellationToken,
}

fn unsupported<'a, T: Send + 'a>(feature: &'static str) -> AgentFuture<'a, T> {
    Box::pin(async move { Err(AgentError::Unsupported(feature.into())) })
}

fn protocol(message: &str) -> AgentError {
    AgentError::Protocol(message.into())
}

pub(crate) async fn read_metadata_output(
    binary: &str,
    arguments: &[&str],
    directory: &str,
    stop: &CancellationToken,
) -> Result<Vec<u8>, AgentError> {
    read_metadata_output_bounded(
        binary,
        arguments,
        directory,
        stop,
        1024 * 1024,
        Duration::from_secs(15),
    )
    .await
}

pub(crate) async fn read_metadata_output_bounded(
    binary: &str,
    arguments: &[&str],
    directory: &str,
    stop: &CancellationToken,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, AgentError> {
    use std::process::Stdio;
    let mut child = tokio::process::Command::new(binary)
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| AgentError::NotAvailable("Could not run Agent metadata command".into()))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Protocol("Agent metadata stdout missing".into()))?
        .take(limit as u64 + 1);
    let mut bytes = Vec::new();
    let result = tokio::select! {
        _ = stop.cancelled() => Err(AgentError::NotAvailable("Agent metadata command cancelled".into())),
        result = tokio::time::timeout(timeout, async {
            stdout.read_to_end(&mut bytes).await.map_err(|_| AgentError::Protocol("Could not read Agent metadata".into()))?;
            if bytes.len() > limit { return Err(AgentError::Protocol("Agent metadata exceeds its byte budget".into())); }
            let status = child.wait().await.map_err(|_| AgentError::NotAvailable("Agent metadata process failed".into()))?;
            if !status.success() { return Err(AgentError::NotAvailable("Agent metadata is unavailable; inspect FX authentication in the terminal".into())); }
            Ok(bytes)
        }) => result.unwrap_or_else(|_| Err(AgentError::NotAvailable("Agent metadata command timed out".into()))),
    };
    if result.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    result
}

impl<P: AcpProvider> AcpDriver<P> {
    pub fn new(
        binary: String,
        default_directory: String,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            provider: PhantomData,
            binary,
            default_directory,
            mirror,
            tx,
            sessions: Arc::new(StdMutex::new(HashMap::new())),
            dormant: Arc::new(StdMutex::new(HashMap::new())),
            idle_timeout_secs: Arc::new(AtomicU64::new(DEFAULT_IDLE_TIMEOUT_SECS)),
            inspection: Arc::new(Mutex::new(())),
            connections: StdMutex::new(Vec::new()),
            connection_admission: Mutex::new(()),
            starting: tokio::sync::RwLock::new(()),
            mutations: Mutex::new(()),
            global_catalog: Mutex::new(None),
            session_metadata: Mutex::new(None),
            metadata_refresh: Mutex::new(()),
            session_lists: Mutex::new(Vec::new()),
            session_store: StdMutex::new(P::session_store_fingerprint),
            list_refreshes: StdMutex::new(HashMap::new()),
            metadata_revision: AtomicU64::new(0),
            native_capabilities: StdMutex::new(Value::Null),
            commands_observed: Arc::new(AtomicBool::new(false)),
            launch: StdMutex::new(None),
            mcp_servers: StdMutex::new(json!([])),
            terminal: StdMutex::new(None),
            me: me.clone(),
            last_catalog: StdMutex::new(None),
            stop: CancellationToken::new(),
        })
    }

    /// Tests point the store probe away from the real native store.
    #[cfg(test)]
    pub(crate) fn set_session_store(&self, probe: fn() -> Option<u64>) {
        *self.session_store.lock().unwrap() = probe;
    }

    /// Replace `ARGS` and add child environment for every later spawn.
    pub fn set_launch(&self, launch: AcpLaunch) {
        *self.launch.lock().unwrap() = Some(launch);
    }

    /// ACP `mcpServers` entries given to every `session/new`, `session/load`
    /// and `session/fork`. Never written to the agent's own configuration.
    pub fn set_mcp_servers(&self, servers: Value) {
        if servers.is_array() {
            *self.mcp_servers.lock().unwrap() = servers;
        }
    }

    fn mcp_servers(&self) -> Value {
        self.mcp_servers.lock().unwrap().clone()
    }

    pub fn set_terminal_bridge(&self, bridge: Arc<dyn TerminalBridge>) {
        *self.terminal.lock().unwrap() = Some(bridge);
    }

    fn terminal_bridge(&self) -> Option<Arc<dyn TerminalBridge>> {
        self.terminal.lock().unwrap().clone()
    }

    /// The terminal holding `id`, if any. A native child this driver holds
    /// for the same session is handed over: released now when idle, else
    /// after its turn (the next observation). Until then both may write --
    /// the window is one turn and is logged.
    async fn terminal_live(&self, id: &str) -> Option<TerminalLive> {
        let bridge = self.terminal_bridge()?;
        let live = bridge.live(&[id.to_owned()]).await.ok()?.remove(id)?;
        if let Some(session) = self.retained(id) {
            if session.busy.load(Ordering::Acquire) {
                tracing::warn!(
                    session = id,
                    "Claude Code session resumed in a terminal while a Gateway turn runs; \
                     handing over after this turn"
                );
                return None;
            }
            tracing::info!(
                session = id,
                "session resumed in a terminal; releasing the Gateway's process"
            );
            self.sessions.lock().unwrap().remove(id);
            session.connected.store(false, Ordering::Release);
            session.client.shutdown().await;
        }
        Some(live)
    }

    /// Put a terminal-held session's transcript into the mirror and tell
    /// readers to refetch.
    /// A terminal-held session's history, read from its transcript. Never
    /// pending approvals or forms: those belong to the terminal.
    async fn terminal_snapshot(
        &self,
        id: &str,
        live: &TerminalLive,
    ) -> Result<AgentSessionSnapshot, AgentError> {
        let mut info = match self.native_session_info(id).await {
            Ok(info) => info,
            Err(error) => self
                .mirror
                .session_info(&AgentSessionId::from(id))
                .await
                .ok_or(error)?,
        };
        let directory = info.directory.clone().ok_or_else(|| {
            AgentError::NotAvailable("The session has no verified working directory".into())
        })?;
        let timeline = P::live_timeline(&directory, id, &self.stop).await?;
        info.runtime = Some(live.runtime.clone());
        live.runtime.clone().apply(&mut info);
        Ok(AgentSessionSnapshot {
            info,
            timeline,
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 0,
        })
    }

    async fn refresh_terminal_snapshot(
        &self,
        id: &str,
        live: &TerminalLive,
    ) -> Result<(), AgentError> {
        let snapshot = self.terminal_snapshot(id, live).await?;
        let revision = self
            .mirror
            .snapshot_read_revision(&snapshot.info.asid)
            .await;
        self.mirror
            .refresh_prepared_snapshot(snapshot, revision.0, revision.1, None)
            .await?;
        Ok(())
    }

    /// Input for a terminal-held session goes into that terminal: the user
    /// there sees it typed. Without a known pane it cannot be delivered.
    async fn send_to_terminal(
        &self,
        id: &str,
        live: TerminalLive,
        text: &str,
        attachments: &[String],
    ) -> Result<(), AgentError> {
        let Some(route) = live.route else {
            return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
        };
        if !attachments.is_empty() {
            return Err(AgentError::Unsupported(
                "attachments for a conversation open in a terminal".into(),
            ));
        }
        let bridge = self
            .terminal_bridge()
            .ok_or_else(|| AgentError::NotAvailable("No terminal bridge".into()))?;
        bridge.send(&route, text).await?;
        // Acknowledge the reader's send as native prompts are; the next
        // transcript refresh replaces this row with the terminal's own.
        let native = format!("terminal-submission:{}", uuid::Uuid::new_v4());
        let row = TimelineItem {
            id: native.clone(),
            message_id: native,
            role: TimelineRole::User,
            part: AgentPart::Text { text: text.into() },
            seq: 0,
            updated_ms: mapper::now_ms(),
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        };
        let event = self
            .mirror
            .record_timeline_upsert(&AgentSessionId::from(id), vec![row])
            .await;
        Self::emit(&self.mirror, &self.tx, event);
        self.follow_terminal(id.to_owned());
        Ok(())
    }

    /// Follow a terminal-held transcript after input from here, so readers
    /// see the turn the terminal runs.
    fn follow_terminal(&self, id: String) {
        let driver = self.me.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut seen = None;
            while started.elapsed() < TERMINAL_FOLLOW {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(driver) = driver.upgrade() else {
                    return;
                };
                if driver.stop.is_cancelled() {
                    return;
                }
                let Some(directory) = driver
                    .native_session_info(&id)
                    .await
                    .ok()
                    .and_then(|info| info.directory)
                else {
                    return;
                };
                let modified = P::transcript_modified(&directory, &id);
                if modified == seen {
                    continue;
                }
                seen = modified;
                let Some(live) = driver.terminal_live(&id).await else {
                    return;
                };
                if driver.refresh_terminal_snapshot(&id, &live).await.is_ok() {
                    Self::emit(
                        &driver.mirror,
                        &driver.tx,
                        AgentDomainEvent::Resync {
                            asid: id.as_str().into(),
                            reason: "terminal transcript advanced".into(),
                        },
                    );
                }
            }
        });
    }

    pub fn set_idle_timeout(&self, timeout: Duration) {
        self.idle_timeout_secs
            .store(timeout.as_secs().max(1), Ordering::Release);
    }

    /// Discovery verifies the binary. ACP is initialized only on an explicit read or mutation.
    pub fn is_connected(&self) -> bool {
        !self.stop.is_cancelled()
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }

    pub fn has_live_stream(&self) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .any(|session| session.connected.load(Ordering::Acquire))
    }

    pub async fn shutdown(&self) {
        self.stop();
        // Initialization registers every private child before releasing this
        // gate, including short-lived listing clients and unfinished creates.
        let _starting = self.starting.write().await;
        let clients: Vec<_> = self
            .connections
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        futures::future::join_all(clients.iter().map(|client| client.shutdown())).await;
        let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
        futures::future::join_all(sessions.iter().map(|session| async move {
            session.client.shutdown().await;
            let _ =
                tokio::time::timeout(Duration::from_secs(5), session.finished.cancelled()).await;
        }))
        .await;
    }

    /// Only catalogs actually received from native sessions contribute choices.
    pub async fn observed_catalog(&self) -> AgentCatalog {
        let mut sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect();
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        let mut models: BTreeMap<(String, String), ModelInfo> = BTreeMap::new();
        let mut commands = BTreeMap::new();
        for (_, session) in sessions {
            if !session.connected.load(Ordering::Acquire) {
                continue;
            }
            let state = session.state.lock().await;
            for command in &state.catalog.commands {
                commands
                    .entry(command.name.clone())
                    .or_insert_with(|| command.clone());
            }
            for model in &state.catalog.models {
                let current = models
                    .entry((model.provider_id.clone(), model.id.clone()))
                    .or_insert_with(|| model.clone());
                if let Some(variants) = &model.variants {
                    let mut known: BTreeMap<_, _> = current
                        .variants
                        .take()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|variant| (variant.id.clone(), variant))
                        .collect();
                    for variant in variants {
                        known
                            .entry(variant.id.clone())
                            .or_insert_with(|| variant.clone());
                    }
                    current.variants = Some(known.into_values().collect());
                }
            }
        }
        AgentCatalog {
            models: models.into_values().collect(),
            commands: commands.into_values().collect(),
            ..Default::default()
        }
    }

    pub fn observed_capabilities(&self) -> Value {
        self.native_capabilities.lock().unwrap().clone()
    }

    /// A verified read-only provider catalog may bootstrap choices before a
    /// session exists. Native session catalogs remain the authoritative source.
    pub async fn available_catalog(
        &self,
        directory: Option<&str>,
    ) -> Result<AgentCatalog, AgentError> {
        let mut observed = self.observed_catalog().await;
        if !P::MODEL_SELECTION {
            observed.models.clear();
            observed.defaults.model = None;
            return Ok(observed);
        }
        if !observed.models.is_empty() {
            *self.last_catalog.lock().unwrap() = Some(observed.clone());
            return Ok(observed);
        }

        let directory = directory.unwrap_or(&self.default_directory);
        let mut cached = self.global_catalog.lock().await;
        if let Some((scope, at, result)) = &*cached {
            let ttl = if result.is_ok() {
                P::CATALOG_TTL
            } else {
                Duration::from_secs(5)
            };
            if scope == directory && at.elapsed() < ttl {
                return Self::merge_global_catalog(result.clone(), observed);
            }
        }
        let result = match P::global_catalog(&self.binary, directory, &self.stop).await {
            // Without a read-only catalog, what a live session last offered.
            Err(AgentError::Unsupported(_)) => Ok(self
                .last_catalog
                .lock()
                .unwrap()
                .clone()
                .map(|mut last| {
                    last.commands.clear();
                    last.defaults.model = None;
                    last
                })
                .unwrap_or_default()),
            result => result,
        };
        *cached = Some((
            directory.into(),
            tokio::time::Instant::now(),
            result.clone(),
        ));
        Self::merge_global_catalog(result, observed)
    }

    fn merge_global_catalog(
        result: Result<AgentCatalog, AgentError>,
        observed: AgentCatalog,
    ) -> Result<AgentCatalog, AgentError> {
        match result {
            Ok(mut catalog) => {
                let mut commands: BTreeMap<_, _> = catalog
                    .commands
                    .into_iter()
                    .map(|command| (command.name.clone(), command))
                    .collect();
                for command in observed.commands {
                    commands.insert(command.name.clone(), command);
                }
                catalog.commands = commands.into_values().collect();
                Ok(catalog)
            }
            Err(_) if !observed.commands.is_empty() => Ok(observed),
            Err(error) => Err(error),
        }
    }

    /// Select the model and effort a loaded session last ran with. The
    /// recorded id is matched exactly, else by family alias; anything not
    /// offered is left at the agent's choice, which the catalog then shows.
    async fn restore_recorded_model(
        &self,
        session: &Session,
        id: &str,
        model: &str,
        effort: Option<&str>,
    ) {
        let (model_choice, effort_choice) = {
            let state = session.state.lock().await;
            let values = |key: &str| -> Vec<String> {
                mapper::config_option(&state.config, key)
                    .map(|option| {
                        mapper::select_options(option)
                            .into_iter()
                            .filter_map(|native| native["value"].as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            (
                mapper::recorded_choice(model, &values("model")),
                effort.filter(|effort| values(P::EFFORT_CONFIG).iter().any(|v| v == effort)),
            )
        };
        match &model_choice {
            Some(choice) => {
                if choice != model {
                    tracing::info!(session = id, recorded = model, selected = %choice, "restored the nearest offered model");
                }
                if let Err(error) = self.select_config(session, "model", choice).await {
                    tracing::warn!(session = id, %error, "could not restore the recorded model");
                }
            }
            None => tracing::info!(
                session = id,
                recorded = model,
                "recorded model is not offered; keeping the agent's default"
            ),
        }
        if let Some(effort) = effort_choice {
            if let Err(error) = self.select_config(session, P::EFFORT_CONFIG, effort).await {
                tracing::warn!(session = id, %error, "could not restore the recorded effort");
            }
        }
    }

    async fn select_config(
        &self,
        session: &Session,
        config_id: &str,
        value: &str,
    ) -> Result<(), AgentError> {
        let mut state = session.state.lock().await;
        state.last_activity = tokio::time::Instant::now();
        let option = mapper::config_option(&state.config, config_id)
            .ok_or_else(|| AgentError::Unsupported(format!("ACP {config_id} selection")))?;
        if option["currentValue"].as_str() == Some(value) {
            return Ok(());
        }
        if !mapper::offers_config_value(&state.config, config_id, value) {
            return Err(AgentError::InvalidRequest(format!(
                "ACP {config_id} choice is not offered by this session"
            )));
        }
        let previous_provider = state
            .info
            .model
            .as_ref()
            .map(|model| model.provider_id.clone());
        let response = session
            .client
            .request_with_timeout(
                "session/set_config_option",
                json!({
                    "sessionId": state.info.asid.0, "configId": config_id, "value": value
                }),
                RPC_TIMEOUT,
            )
            .await;
        self.invalidate_session_lists().await;
        let response = response.map_err(AgentError::RequestFailed)?;
        if !response["configOptions"].is_array() {
            return Err(protocol(
                "ACP config selection returned no configOptions; refresh before retrying",
            ));
        }
        let update = json!({"sessionUpdate":"config_option_update","configOptions":response["configOptions"]});
        Self::reduce(&mut state, &update)?;
        let info = state.info.clone();
        let seq = self.mirror.update_session(info.clone()).await;
        Self::emit(
            &self.mirror,
            &self.tx,
            AgentDomainEvent::SessionUpdated {
                asid: info.asid.clone(),
                info: Box::new(info),
                seq,
            },
        );
        if config_id != "provider"
            && state
                .info
                .model
                .as_ref()
                .map(|model| model.provider_id.clone())
                != previous_provider
        {
            return Err(protocol("ACP changed the provider during model or effort selection; native state was retained, refresh before retrying"));
        }
        if mapper::config_option(&state.config, config_id)
            .and_then(|option| option["currentValue"].as_str())
            != Some(value)
        {
            return Err(protocol(
                "ACP did not confirm the requested config value; refresh before retrying",
            ));
        }
        Ok(())
    }

    async fn select_model(&self, session: &Session, model: &ModelRef) -> Result<(), AgentError> {
        if !P::MODEL_SELECTION {
            return Err(AgentError::Unsupported("ACP model selection".into()));
        }
        if !session.connected.load(Ordering::Acquire) {
            return Err(AgentError::NotAvailable("ACP connection ended".into()));
        }
        if session
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(AgentError::NotAvailable(
                "ACP session is busy; model changes require an idle session".into(),
            ));
        }
        let _admission = Admission {
            busy: &session.busy,
            handed_off: false,
        };
        if P::PROVIDER_CONFIG {
            self.select_config(session, "provider", &model.provider_id)
                .await?;
        } else if model.provider_id != P::KIND {
            return Err(AgentError::InvalidRequest(
                "model belongs to another ACP provider".into(),
            ));
        }
        let native_model = {
            let state = session.state.lock().await;
            mapper::resolve_model_value(&state.config, &model.model_id)?
        };
        self.select_config(session, "model", &native_model).await?;
        if let Some(effort) = &model.variant {
            self.select_config(session, P::EFFORT_CONFIG, effort)
                .await?;
        }
        Ok(())
    }

    async fn initialized(
        &self,
        directory: &str,
    ) -> Result<(Arc<AcpClient>, broadcast::Receiver<AcpEvent>, Value), AgentError> {
        let _starting = self.starting.read().await;
        if self.stop.is_cancelled() {
            return Err(AgentError::NotAvailable("ACP runtime stopped".into()));
        }
        // Reserve two short-lived listing connections beyond the session cap.
        // Serialize spawn admission only; prompts and initialization stay concurrent.
        let admission = self.connection_admission.lock().await;
        {
            let mut connections = self.connections.lock().unwrap();
            connections.retain(|client| client.upgrade().is_some_and(|client| client.is_running()));
            if connections.len() >= MAX_CONNECTIONS + 2 {
                return Err(AgentError::NotAvailable(
                    "ACP connection capacity exhausted".into(),
                ));
            }
        }
        let launch = self.launch.lock().unwrap().clone().unwrap_or_default();
        let args: Vec<&str> = if launch.args.is_empty() {
            P::ARGS.to_vec()
        } else {
            launch.args.iter().map(String::as_str).collect()
        };
        let client = AcpClient::spawn_with_env(
            &self.binary,
            &args,
            &launch.env,
            directory,
            P::LABEL,
            P::safe_error_message,
        )
        .await
        .map_err(AgentError::NotAvailable)?;
        {
            let mut connections = self.connections.lock().unwrap();
            connections.retain(|client| client.upgrade().is_some_and(|client| client.is_running()));
            connections.push(Arc::downgrade(&client));
        }
        drop(admission);
        let events = client.subscribe();
        let mut client_capabilities = json!({
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false
        });
        if P::SESSION_NOTICES {
            client_capabilities["session"] = json!({ "notices": {} });
        }
        let initialize = client.request_with_timeout(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": { "name": "muqun-gateway", "version": env!("CARGO_PKG_VERSION") },
                "clientCapabilities": client_capabilities
            }),
            P::INITIALIZE_TIMEOUT,
        );
        let result = tokio::select! {
            _ = self.stop.cancelled() => Err("ACP runtime stopped".into()),
            result = initialize => result,
        };
        match result {
            Ok(value) if value["protocolVersion"].as_u64() == Some(1) => {
                let capabilities = value["agentCapabilities"].clone();
                *self.native_capabilities.lock().unwrap() = capabilities.clone();
                Ok((client, events, capabilities))
            }
            result => {
                client.shutdown().await;
                Err(match result {
                    Ok(_) => protocol("ACP did not negotiate ACP protocol 1"),
                    Err(error) => AgentError::Protocol(error),
                })
            }
        }
    }

    fn retained(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    async fn native_sessions(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        self.native_sessions_with_timeout(directory, RPC_TIMEOUT)
            .await
    }

    async fn native_sessions_with_timeout(
        &self,
        directory: Option<&str>,
        timeout: Duration,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        // Queueing, startup and every page share one budget. Failed refreshes
        // must not give each queued caller another full native RPC timeout.
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(AgentError::NotAvailable("ACP runtime stopped".into())),
            result = tokio::time::timeout(timeout, self.refresh_session_list(directory)) => {
                result.unwrap_or_else(|_| Err(AgentError::NotAvailable("ACP session listing timed out".into())))
            }
        }
    }

    async fn refresh_session_list(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        if self.stop.is_cancelled() {
            return Err(AgentError::NotAvailable("ACP runtime stopped".into()));
        }
        // Taken before listing: a store change during the read invalidates it.
        let probe = *self.session_store.lock().unwrap();
        let fingerprint = tokio::task::spawn_blocking(probe).await.ok().flatten();
        if let Some(sessions) = self.cached_session_list(directory, fingerprint).await {
            return Ok(sessions);
        }
        // Equal scopes share one native startup; unrelated directories do not
        // wait behind each other. Weak gates disappear after their last caller.
        let scope = directory.map(str::to_owned);
        let gate = {
            let mut gates = self.list_refreshes.lock().unwrap();
            gates.retain(|_, gate| gate.strong_count() > 0);
            if let Some(gate) = gates.get(&scope).and_then(Weak::upgrade) {
                gate
            } else {
                if gates.len() >= MAX_CONNECTIONS + 2 {
                    return Err(AgentError::NotAvailable(
                        "ACP metadata request capacity exhausted".into(),
                    ));
                }
                let gate = Arc::new(Mutex::new(()));
                gates.insert(scope, Arc::downgrade(&gate));
                gate
            }
        };
        let _refresh = tokio::select! {
            _ = self.stop.cancelled() => return Err(AgentError::NotAvailable("ACP runtime stopped".into())),
            guard = gate.lock() => guard,
        };
        if let Some(sessions) = self.cached_session_list(directory, fingerprint).await {
            return Ok(sessions);
        }
        let revision = self.metadata_revision.load(Ordering::Acquire);
        let sessions = self.read_native_sessions(directory).await?;
        let mut lists = self.session_lists.lock().await;
        if self.metadata_revision.load(Ordering::Acquire) == revision {
            lists.retain(|list| list.fresh(fingerprint) && list.directory.as_deref() != directory);
            while !lists.is_empty()
                && (lists.len() >= MAX_LIST_SCOPES
                    || lists.iter().map(|list| list.sessions.len()).sum::<usize>() + sessions.len()
                        > MAX_CACHED_LIST_ROWS)
            {
                lists.remove(0);
            }
            if sessions.len() <= MAX_CACHED_LIST_ROWS {
                lists.push(CachedSessionList {
                    directory: directory.map(str::to_owned),
                    listed_at: tokio::time::Instant::now(),
                    fingerprint,
                    sessions: sessions.clone(),
                });
            }
        }
        Ok(sessions)
    }

    async fn cached_session_list(
        &self,
        directory: Option<&str>,
        fingerprint: Option<u64>,
    ) -> Option<Vec<AgentSessionInfo>> {
        let mut sessions = {
            let mut lists = self.session_lists.lock().await;
            lists.retain(|list| list.fresh(fingerprint));
            lists
                .iter()
                .find(|list| list.directory.as_deref() == directory)?
                .sessions
                .clone()
        };
        // A metadata snapshot never freezes a Gateway-owned session's live
        // status, model, title, permissions or latest activity.
        for info in &mut sessions {
            if let Some(known) = self.retained(&info.asid.0) {
                *info = known.state.lock().await.info.clone();
            } else if let Some(known) = self.dormant.lock().unwrap().get(&info.asid.0) {
                known.overlay(info);
            }
        }
        Some(sessions)
    }

    async fn invalidate_session_lists(&self) {
        self.metadata_revision.fetch_add(1, Ordering::AcqRel);
        self.session_lists.lock().await.clear();
        *self.session_metadata.lock().await = None;
    }

    async fn read_native_sessions(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        let revision = self.metadata_revision.load(Ordering::Acquire);
        if let Some(rows) = P::offline_sessions(directory, &self.stop) {
            let mut sessions = self.project_listed_sessions(rows.await?).await?;
            if let Some(bridge) = self.terminal_bridge() {
                let ids: Vec<_> = sessions.iter().map(|info| info.asid.0.clone()).collect();
                let mut live = bridge.live(&ids).await.unwrap_or_default();
                for info in &mut sessions {
                    if let Some(live) = live.remove(&info.asid.0) {
                        live.runtime.clone().apply(info);
                        info.runtime = Some(live.runtime);
                    }
                }
            }
            self.remember_session_metadata(revision, &sessions).await;
            return Ok(sessions);
        }
        let (client, _events, capabilities) = self.initialized(&self.default_directory).await?;
        if !capabilities
            .pointer("/sessionCapabilities/list")
            .is_some_and(Value::is_object)
        {
            client.shutdown().await;
            let sessions: Vec<_> = self.sessions.lock().unwrap().values().cloned().collect();
            let mut retained = Vec::new();
            for session in sessions {
                let info = session.state.lock().await.info.clone();
                if directory.is_none_or(|cwd| info.directory.as_deref() == Some(cwd)) {
                    retained.push(info);
                }
            }
            return Ok(retained);
        }
        let result = async {
            let mut cursor = None;
            let mut seen = HashSet::new();
            let mut sessions = Vec::new();
            let mut listed = HashSet::new();
            let mut observed_bytes: usize = 0;
            for _ in 0..MAX_LIST_PAGES {
                let mut params = json!({});
                if let Some(directory) = directory {
                    params["cwd"] = json!(directory);
                }
                if let Some(cursor) = &cursor {
                    params["cursor"] = json!(cursor);
                }
                let value = client
                    .request_with_timeout("session/list", params, RPC_TIMEOUT)
                    .await
                    .map_err(AgentError::RequestFailed)?;
                observed_bytes = observed_bytes.saturating_add(
                    serde_json::to_vec(&value)
                        .map(|bytes| bytes.len())
                        .unwrap_or(usize::MAX),
                );
                if observed_bytes > 8 * 1024 * 1024 {
                    return Err(AgentError::NotAvailable(
                        "ACP session listing exceeded its byte budget".into(),
                    ));
                }
                let rows = value["sessions"]
                    .as_array()
                    .ok_or_else(|| protocol("ACP list missing sessions"))?;
                for row in rows {
                    let id = row["sessionId"]
                        .as_str()
                        .ok_or_else(|| protocol("ACP listed session missing sessionId"))?;
                    row["cwd"]
                        .as_str()
                        .ok_or_else(|| protocol("ACP listed session missing cwd"))?;
                    if !listed.insert(id.to_owned()) {
                        continue;
                    }
                    if sessions.len() >= 4096 {
                        return Err(AgentError::NotAvailable(
                            "ACP session listing exceeded its session budget".into(),
                        ));
                    }
                    sessions.push(row.clone());
                }
                let Some(next) = value["nextCursor"].as_str() else {
                    return self.project_listed_sessions(sessions).await;
                };
                if next.is_empty() || !seen.insert(next.to_owned()) {
                    return Err(protocol("ACP session cursor did not advance"));
                }
                cursor = Some(next.to_owned());
            }
            Err(AgentError::NotAvailable(
                "ACP session listing exceeded its page budget".into(),
            ))
        }
        .await;
        client.shutdown().await;
        if let Ok(sessions) = &result {
            self.remember_session_metadata(revision, sessions).await;
        }
        result
    }

    async fn remember_session_metadata(&self, revision: u64, sessions: &[AgentSessionInfo]) {
        let mut metadata = self.session_metadata.lock().await;
        if self.metadata_revision.load(Ordering::Acquire) == revision {
            *metadata = Some(SessionMetadata {
                listed_at: tokio::time::Instant::now(),
                sessions: sessions
                    .iter()
                    .map(|info| (info.asid.0.clone(), info.clone()))
                    .collect(),
            });
        }
    }

    async fn project_listed_sessions(
        &self,
        rows: Vec<Value>,
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        let mut expected: HashSet<_> = rows
            .iter()
            .filter_map(|row| row["sessionId"].as_str().map(str::to_owned))
            .collect();
        let rows = P::session_metadata(rows, &self.stop).await?;
        for row in &rows {
            let id = row["sessionId"]
                .as_str()
                .ok_or_else(|| protocol("ACP session metadata missing identity"))?;
            if !expected.remove(id) {
                return Err(protocol("ACP session metadata changed listed identities"));
            }
        }
        if !expected.is_empty() {
            return Err(protocol("ACP session metadata dropped listed identities"));
        }
        let mut sessions = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row["sessionId"].as_str().unwrap();
            let cwd = row["cwd"].as_str().unwrap_or_default();
            let mut info = mapper::session(id, cwd, &row);
            if !std::path::Path::new(cwd).is_absolute() {
                info.directory = None;
            }
            // A native listing proves neither runtime ownership nor idle state.
            info.status = AgentSessionStatus::Unknown;
            let dormant = self.dormant.lock().unwrap().get(id).cloned();
            if let Some(known) = self.retained(id) {
                let mut state = known.state.lock().await;
                let mut changed = false;
                if !known.busy.load(Ordering::Acquire) && row["title"].is_string() {
                    changed = state.info.title != info.title;
                    state.info.title = info.title;
                    state.info.updated_ms = state.info.updated_ms.max(info.updated_ms);
                }
                info = state.info.clone();
                if changed {
                    let seq = self.mirror.update_session(info.clone()).await;
                    Self::emit(
                        &self.mirror,
                        &self.tx,
                        AgentDomainEvent::SessionUpdated {
                            asid: info.asid.clone(),
                            info: Box::new(info.clone()),
                            seq,
                        },
                    );
                }
            } else if let Some(known) = dormant {
                known.overlay(&mut info);
            } else if info.directory.is_none() {
                info.directory = self
                    .mirror
                    .session_info(&info.asid)
                    .await
                    .and_then(|known| known.directory)
                    .filter(|cwd| std::path::Path::new(cwd).is_absolute());
            }
            if P::KIND == "qoder"
                && info.directory.is_none()
                && row["_muqunEmptySession"].as_bool() == Some(true)
            {
                info.runtime = Some(Self::empty_runtime());
            }
            sessions.push(info);
        }
        Ok(sessions)
    }

    async fn cached_session_info(&self, id: &str) -> Option<AgentSessionInfo> {
        self.session_metadata
            .lock()
            .await
            .as_ref()
            .filter(|metadata| metadata.listed_at.elapsed() < SESSION_METADATA_TTL)
            .and_then(|metadata| metadata.sessions.get(id).cloned())
    }

    async fn native_session_info(&self, id: &str) -> Result<AgentSessionInfo, AgentError> {
        if let Some(info) = self.cached_session_info(id).await {
            return Ok(info);
        }
        let _refresh = self.metadata_refresh.lock().await;
        if let Some(info) = self.cached_session_info(id).await {
            return Ok(info);
        }
        // An inventory snapshot's absence must not become a cached claim that
        // an exact native identity was deleted or never existed.
        self.read_native_sessions(None)
            .await?
            .into_iter()
            .find(|info| info.asid.0 == id)
            .ok_or_else(|| AgentError::SessionNotFound(id.into()))
    }

    fn empty_runtime() -> SessionRuntime {
        SessionRuntime {
            transcript: Some(TranscriptSource::Persisted),
            source: RuntimeSource::Native,
            interaction: RuntimeInteraction::Unavailable,
            status: Some(TerminalRuntimeStatus::Unknown),
            terminal: None,
            reason: Some(RuntimeReason::NotLoaded),
        }
    }

    async fn readonly_empty_snapshot(&self, id: &str) -> Result<bool, AgentError> {
        if self.retained(id).is_some() || self.dormant.lock().unwrap().contains_key(id) {
            return Ok(false);
        }
        // A terminal-held session is read from its transcript instead.
        if self.terminal_live(id).await.is_some() {
            return Ok(false);
        }
        let info = self.native_session_info(id).await?;
        if info.directory.is_some() || info.runtime.as_ref() != Some(&Self::empty_runtime()) {
            return Ok(false);
        }
        if let Some(snapshot) = self.mirror.get_snapshot(&info.asid).await {
            if !snapshot.timeline.is_empty()
                || !snapshot.permissions.is_empty()
                || !snapshot.forms.is_empty()
                || !snapshot.inbox.is_empty()
            {
                return Err(AgentError::NotAvailable(
                    "Native empty-session metadata conflicts with the retained conversation".into(),
                ));
            }
            return Ok(true);
        }
        let revision = self.mirror.snapshot_read_revision(&info.asid).await;
        let snapshot = AgentSessionSnapshot {
            info,
            timeline: Vec::new(),
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 0,
        };
        if self
            .mirror
            .refresh_prepared_snapshot(snapshot, revision.0, revision.1, None)
            .await?
            .is_none()
        {
            return Err(AgentError::NotAvailable(
                "Empty-session snapshot changed; refresh again".into(),
            ));
        }
        Ok(true)
    }

    async fn ensure_loaded(&self, id: &str) -> Result<Arc<Session>, AgentError> {
        self.activate(id, false, false).await
    }

    async fn activate(
        &self,
        id: &str,
        explicit_prepare: bool,
        reserve_prompt: bool,
    ) -> Result<Arc<Session>, AgentError> {
        // A terminal holds it: never a second agent process.
        if let Some(live) = self.terminal_live(id).await {
            return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
        }
        // One explicit activation restores a same-ID generation. Prompt calls
        // never replay a previous request and cannot recover an unexpected loss.
        let _mutation = self.mutations.lock().await;
        if let Some(session) = self.retained(id) {
            let retirement = session.retirement.lock().await;
            if session.connected.load(Ordering::Acquire) {
                if reserve_prompt
                    && session
                        .busy
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                {
                    return Err(AgentError::NotAvailable(
                        "ACP session is already processing a turn".into(),
                    ));
                }
                if explicit_prepare {
                    session.state.lock().await.last_activity = tokio::time::Instant::now();
                }
                drop(retirement);
                return Ok(session);
            }
            drop(retirement);
            tokio::time::timeout(RETIREMENT_TIMEOUT, session.finished.cancelled())
                .await
                .map_err(|_| {
                    AgentError::NotAvailable("Previous ACP generation has not stopped".into())
                })?;
        }
        let dormant = self.dormant.lock().unwrap().get(id).cloned();
        if dormant
            .as_ref()
            .is_some_and(|entry| entry.kind == DormantKind::Unexpected)
            && !explicit_prepare
        {
            return Err(AgentError::NotAvailable("ACP connection ended unexpectedly; explicitly open the conversation before sending a new prompt; delivery was not retried".into()));
        }
        let info = match &dormant {
            Some(entry) => self.dormant_info(id, entry).await?,
            None => self.native_session_info(id).await?,
        };
        let directory = info.directory.clone()
            .filter(|cwd| std::path::Path::new(cwd).is_absolute())
            .ok_or_else(|| AgentError::NotAvailable("The native session has no verified working directory; open it in Qoder or FX once before loading it here".into()))?;
        let session = self.open(&directory, Some(info)).await?;
        if dormant
            .as_ref()
            .is_none_or(|entry| entry.info.model.is_none())
        {
            if let Ok(Some((model, effort))) = P::transcript_model(&directory, id).await {
                self.restore_recorded_model(&session, id, &model, effort.as_deref())
                    .await;
            }
        }
        if let Some(entry) = dormant {
            session.state.lock().await.owned_mutations = entry.owned_mutations;
            if let Some(model) = &entry.info.model {
                if session.state.lock().await.info.model.as_ref() != Some(model) {
                    // Model/effort choices are session-scoped. Restore the
                    // previous offered selection without changing host defaults.
                    self.select_model(&session, model).await?;
                }
            }
            self.dormant.lock().unwrap().remove(id);
        }
        if reserve_prompt
            && session
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(AgentError::NotAvailable(
                "ACP session is already processing a turn".into(),
            ));
        }
        Ok(session)
    }

    async fn observed_snapshot(
        &self,
        id: &str,
    ) -> Result<Option<AgentSessionSnapshot>, AgentError> {
        if self.retained(id).is_some() {
            return Ok(None);
        }
        if self.dormant.lock().unwrap().contains_key(id) {
            return self
                .mirror
                .get_snapshot(&AgentSessionId::from(id))
                .await
                .map(Some)
                .ok_or_else(|| {
                    AgentError::NotAvailable(
                        "Cached history expired; explicitly open the conversation to restore it"
                            .into(),
                    )
                });
        }
        if let Some(live) = self.terminal_live(id).await {
            return self.terminal_snapshot(id, &live).await.map(Some);
        }
        Err(AgentError::NotAvailable(
            "Conversation history is not loaded; explicitly open it before reading native history"
                .into(),
        ))
    }

    async fn dormant_info(
        &self,
        id: &str,
        entry: &DormantSession,
    ) -> Result<AgentSessionInfo, AgentError> {
        if entry.complete_metadata {
            return Ok(entry.info.clone());
        }
        let mut info = match self.mirror.session_info(&AgentSessionId::from(id)).await {
            Some(info) => info,
            None => self.native_session_info(id).await?,
        };
        entry.overlay(&mut info);
        Ok(info)
    }

    async fn open(
        &self,
        directory: &str,
        existing: Option<AgentSessionInfo>,
    ) -> Result<Arc<Session>, AgentError> {
        {
            let sessions = self.sessions.lock().unwrap();
            let dormant = self.dormant.lock().unwrap();
            if dormant.len() + sessions.len() >= MAX_DORMANT_SESSIONS
                && existing
                    .as_ref()
                    .is_none_or(|info| !dormant.contains_key(&info.asid.0))
            {
                return Err(AgentError::NotAvailable(
                    "ACP recovery metadata capacity exhausted".into(),
                ));
            }
            if sessions.len() >= MAX_CONNECTIONS {
                return Err(AgentError::NotAvailable(
                    "ACP active-session capacity exhausted".into(),
                ));
            }
        }
        let new_session = existing.is_none();
        let id = existing.as_ref().map(|info| info.asid.0.clone());
        let revision = match &id {
            Some(id) => {
                self.mirror
                    .native_reload_revision(&AgentSessionId::from(id.as_str()))
                    .await
            }
            None => Default::default(),
        };
        if let Some(id) = &id {
            if let Some(live) = self.terminal_live(id).await {
                return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
            }
        }
        let (client, mut events, capabilities) = self.initialized(directory).await?;
        if id.is_some() && capabilities["loadSession"].as_bool() != Some(true) {
            client.shutdown().await;
            return Err(AgentError::Unsupported(
                "ACP native session history loading".into(),
            ));
        }
        let method = if id.is_some() {
            "session/load"
        } else {
            "session/new"
        };
        let mut params = json!({ "cwd": directory, "mcpServers": self.mcp_servers() });
        if let Some(id) = &id {
            params["sessionId"] = json!(id);
        }
        let request = client
            .start_request(method, params)
            .await
            .map_err(AgentError::RequestFailed)?;
        let response = request.wait_with_timeout(RPC_TIMEOUT);
        tokio::pin!(response);
        let mut replay = Vec::new();
        let mut replay_bytes: usize = 0;
        // Drain as the full load replays, instead of leaving the bounded transport
        // subscription full while waiting for the response.
        let value = loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(AcpEvent::Notification { method, params }) if method == "session/update" => {
                        replay_bytes = replay_bytes.saturating_add(serde_json::to_vec(&params).map(|bytes| bytes.len()).unwrap_or(usize::MAX));
                        if replay_bytes > 32 * 1024 * 1024 || replay.len() >= 100_000 {
                            client.shutdown().await;
                            return Err(AgentError::NotAvailable("ACP whole-history replay exceeds the Gateway budget; open this session in the agent's terminal".into()));
                        }
                        replay.push(params);
                    },
                    Ok(AcpEvent::Disconnected { reason }) => { client.shutdown().await; return Err(AgentError::Network(reason)); },
                    Ok(AcpEvent::Request { .. }) => {
                        client.shutdown().await;
                        return Err(AgentError::NotAvailable("ACP requested user interaction before the session was loaded; open it in the agent's terminal".into()));
                    },
                    Ok(_) => {},
                    Err(_) => { client.shutdown().await; return Err(protocol("ACP history stream was interrupted")); },
                },
                value = &mut response => break value,
            }
        };
        let value = match value {
            Ok(value) => value,
            Err(error) => {
                client.shutdown().await;
                return Err(AgentError::RequestFailed(error));
            }
        };
        while let Ok(event) = events.try_recv() {
            match event {
                AcpEvent::Notification { method, params } if method == "session/update" => {
                    replay_bytes = replay_bytes.saturating_add(
                        serde_json::to_vec(&params)
                            .map(|bytes| bytes.len())
                            .unwrap_or(usize::MAX),
                    );
                    if replay_bytes > 32 * 1024 * 1024 || replay.len() >= 100_000 {
                        client.shutdown().await;
                        return Err(AgentError::NotAvailable(
                            "ACP whole-history replay exceeds the Gateway budget".into(),
                        ));
                    }
                    replay.push(params);
                }
                AcpEvent::Disconnected { reason } => {
                    client.shutdown().await;
                    return Err(AgentError::Network(reason));
                }
                AcpEvent::Request { .. } => {
                    client.shutdown().await;
                    return Err(AgentError::NotAvailable("ACP requested interaction before session load completed; use the agent's terminal".into()));
                }
                _ => {}
            }
        }
        let id = id.or_else(|| value["sessionId"].as_str().map(str::to_owned)).ok_or_else(|| protocol("ACP session creation returned no sessionId; do not retry creation automatically"))?;
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            client.shutdown().await;
            return Err(protocol(
                "ACP returned an invalid native session identity; do not retry creation",
            ));
        }
        let mut info = existing.unwrap_or_else(|| mapper::session(&id, directory, &value));
        info.status = AgentSessionStatus::Idle;
        info.runtime = None;
        info.error = None;
        info.model = mapper::selected_model_for(&value, P::KIND);
        if info.updated_ms == 0 {
            info.updated_ms = mapper::now_ms();
        }
        info.mode = value
            .pointer("/modes/currentModeId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let (completions, completed) = mpsc::unbounded_channel();
        let mut state = SessionState {
            info,
            catalog: mapper::catalog_for(&value, P::KIND),
            config: value.clone(),
            owned_mutations: new_session,
            usage: mapper::SessionUsage::new(new_session),
            projection: Projection::new(P::REQUIRE_MESSAGE_ID),
            permissions: HashMap::new(),
            last_activity: tokio::time::Instant::now(),
            recovery: None,
            prompt_checkpoint: None,
            prompt_user_digest: None,
            turn_had_output: false,
        };
        for params in replay {
            if params["sessionId"].as_str() != Some(id.as_str()) {
                client.shutdown().await;
                return Err(protocol("ACP history replay changed session identity"));
            }
            if let Err(error) = Self::reduce(&mut state, &params["update"]) {
                client.shutdown().await;
                return Err(error);
            }
        }
        if self.stop.is_cancelled() {
            client.shutdown().await;
            return Err(AgentError::NotAvailable(
                "ACP runtime stopped while loading a session".into(),
            ));
        }
        if P::IDLE_RETIREMENT {
            let _inspection = self.inspection.lock().await;
            if let Ok(snapshot) =
                P::recovery_snapshot(&self.binary, directory, &id, &self.stop).await
            {
                if snapshot.session_id == id
                    && Self::snapshot_matches_projection(&snapshot, &state.projection)
                {
                    state.recovery = Some(RecoveryEvidence::Loaded(snapshot.checkpoint()));
                }
            }
        }
        // Native replay ids need not survive a load. Only a fully validated
        // replacement may replace the old snapshot; no replay row is broadcast.
        let snapshot = AgentSessionSnapshot {
            info: state.info.clone(),
            timeline: state
                .projection
                .timeline()
                .into_iter()
                .map(|mut row| {
                    row.seq = 0;
                    row
                })
                .collect(),
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 0,
        };
        if !self
            .mirror
            .replace_native_snapshot(snapshot, revision)
            .await?
        {
            client.shutdown().await;
            return Err(AgentError::NotAvailable(
                "ACP session changed while loading; refresh again".into(),
            ));
        }
        if !state.catalog.commands.is_empty() {
            self.commands_observed.store(true, Ordering::Release);
        }
        state.last_activity = tokio::time::Instant::now();
        let session = Arc::new(Session {
            client,
            state: Mutex::new(state),
            busy: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            completions,
            prompt_capabilities: PromptCapabilities {
                images: capabilities
                    .pointer("/promptCapabilities/image")
                    .and_then(Value::as_bool)
                    == Some(true),
                embedded_context: capabilities
                    .pointer("/promptCapabilities/embeddedContext")
                    .and_then(Value::as_bool)
                    == Some(true),
            },
            native_capabilities: capabilities,
            commands_observed: self.commands_observed.clone(),
            finished: CancellationToken::new(),
            retirement: Mutex::new(()),
        });
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        Self::emit(
            &self.mirror,
            &self.tx,
            AgentDomainEvent::Resync {
                asid: id.as_str().into(),
                reason: "acp_session_loaded".into(),
            },
        );
        Self::listen(
            session.clone(),
            ListenerContext {
                sessions: self.sessions.clone(),
                dormant: self.dormant.clone(),
                idle_timeout_secs: self.idle_timeout_secs.clone(),
                binary: self.binary.clone(),
                inspection: self.inspection.clone(),
            },
            events,
            completed,
            self.mirror.clone(),
            self.tx.clone(),
            self.stop.child_token(),
        );
        Ok(session)
    }

    fn emit(
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        mut event: AgentDomainEvent,
    ) {
        mirror.stamp_event(&mut event);
        let _ = tx.send(event);
    }

    async fn status(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        status: AgentSessionStatus,
        error: Option<AgentErrorInfo>,
    ) {
        let asid = {
            let mut state = session.state.lock().await;
            state.info.status = status;
            state.info.error = error.clone();
            state.info.updated_ms = mapper::now_ms();
            state.info.asid.clone()
        };
        if let Some(seq) = mirror.update_status(&asid, status, error.clone()).await {
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::StatusChanged {
                    asid,
                    status,
                    error,
                    seq,
                },
            );
        }
    }

    fn reduce(
        state: &mut SessionState,
        update: &Value,
    ) -> Result<Option<TimelineItem>, AgentError> {
        match update["sessionUpdate"].as_str() {
            Some("session_info_update") => {
                if let Some(title) = update["title"].as_str() {
                    state.info.title = title.into();
                }
                Ok(None)
            }
            Some("usage_update") => {
                if let Some(used) = update["used"].as_u64() {
                    state.info.context_usage = Some(SessionContextUsage {
                        tokens: None,
                        total_tokens: used,
                        limit: update["size"].as_u64().map(|size| json!({"context":size})),
                    });
                }
                Ok(None)
            }
            Some("available_commands_update") => {
                state.catalog.commands = operations::commands(update)?;
                Ok(None)
            }
            Some("current_mode_update") => {
                state.info.mode = update["currentModeId"].as_str().map(str::to_owned);
                Ok(None)
            }
            Some("config_option_update") => {
                state.config["configOptions"] = update["configOptions"].clone();
                let mut catalog = mapper::catalog_for(&state.config, P::KIND);
                catalog.commands = state.catalog.commands.clone();
                catalog.modes = state.catalog.modes.clone();
                catalog.defaults.mode = state.catalog.defaults.mode.clone();
                state.catalog = catalog;
                state.config["configOptions"] = update["configOptions"].clone();
                state.info.model = mapper::selected_model_for(&state.config, P::KIND);
                Ok(None)
            }
            _ => state.projection.update(update),
        }
    }

    async fn notification(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        params: &Value,
    ) {
        let mut state = session.state.lock().await;
        let asid = state.info.asid.clone();
        if params["sessionId"].as_str() != Some(asid.0.as_str()) {
            session.connected.store(false, Ordering::Release);
            state.info.status = AgentSessionStatus::Unknown;
            if let Some(seq) = mirror
                .update_status(&asid, AgentSessionStatus::Unknown, None)
                .await
            {
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::StatusChanged {
                        asid: asid.clone(),
                        status: AgentSessionStatus::Unknown,
                        error: None,
                        seq,
                    },
                );
            }
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::Resync {
                    asid,
                    reason: "ACP event changed session identity".into(),
                },
            );
            return;
        }
        if session.busy.load(Ordering::Acquire)
            && params["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
            && params["update"]["content"]["text"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty())
        {
            state.turn_had_output = true;
        }
        let old_info = state.info.clone();
        let old_commands = state.catalog.commands.clone();
        match Self::reduce(&mut state, &params["update"]) {
            Ok(Some(row)) => {
                let event = mirror.record_timeline_upsert(&asid, vec![row]).await;
                Self::emit(mirror, tx, event);
            }
            Ok(None) if old_commands != state.catalog.commands => {
                if !state.catalog.commands.is_empty() {
                    session.commands_observed.store(true, Ordering::Release);
                }
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::Resync {
                        asid,
                        reason: "agent_catalog_changed".into(),
                    },
                );
            }
            Ok(None) if old_info != state.info => {
                let info = state.info.clone();
                let seq = mirror.update_session(info.clone()).await;
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::SessionUpdated {
                        asid,
                        info: Box::new(info),
                        seq,
                    },
                );
            }
            Ok(None) => {}
            Err(error) => {
                session.connected.store(false, Ordering::Release);
                state.info.status = AgentSessionStatus::Unknown;
                let error_info = Some(AgentErrorInfo {
                    name: "AcpHistoryUnavailable".into(),
                    message: error.to_string(),
                    status: None,
                    retry_after_ms: None,
                });
                state.info.error = error_info.clone();
                if let Some(seq) = mirror
                    .update_status(&asid, AgentSessionStatus::Unknown, error_info.clone())
                    .await
                {
                    Self::emit(
                        mirror,
                        tx,
                        AgentDomainEvent::StatusChanged {
                            asid: asid.clone(),
                            status: AgentSessionStatus::Unknown,
                            error: error_info,
                            seq,
                        },
                    );
                }
                Self::emit(
                    mirror,
                    tx,
                    AgentDomainEvent::Resync {
                        asid,
                        reason: error.to_string(),
                    },
                );
            }
        }
    }

    async fn event(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        event: AcpEvent,
    ) -> bool {
        match event {
            AcpEvent::Notification { method, params } if method == "session/update" => {
                Self::notification(session, mirror, tx, &params).await
            }
            AcpEvent::Request { id, method, params } if method == "session/request_permission" => {
                let mut state = session.state.lock().await;
                if params["sessionId"].as_str() != Some(state.info.asid.0.as_str()) {
                    let _ = session
                        .client
                        .respond_error(id, -32602, "Session identity mismatch")
                        .await;
                    return true;
                }
                match state.projection.permission(id.clone(), &params) {
                    Ok(mut permission) => {
                        let mut tool = params["toolCall"].clone();
                        tool["sessionUpdate"] = json!("tool_call");
                        match state.projection.update(&tool) {
                            Ok(Some(row)) => {
                                permission.request.source_message_id = Some(row.message_id.clone());
                                let event = mirror
                                    .record_timeline_upsert(&state.info.asid, vec![row])
                                    .await;
                                Self::emit(mirror, tx, event);
                            }
                            Ok(None) => {}
                            Err(error) => {
                                session.connected.store(false, Ordering::Release);
                                state.info.status = AgentSessionStatus::Unknown;
                                let asid = state.info.asid.clone();
                                if let Some(seq) = mirror
                                    .update_status(&asid, AgentSessionStatus::Unknown, None)
                                    .await
                                {
                                    Self::emit(
                                        mirror,
                                        tx,
                                        AgentDomainEvent::StatusChanged {
                                            asid: asid.clone(),
                                            status: AgentSessionStatus::Unknown,
                                            error: None,
                                            seq,
                                        },
                                    );
                                }
                                Self::emit(
                                    mirror,
                                    tx,
                                    AgentDomainEvent::Resync {
                                        asid,
                                        reason: error.to_string(),
                                    },
                                );
                                return false;
                            }
                        }
                        let request = permission.request.clone();
                        state.permissions.insert(request.id.clone(), permission);
                        let seq = mirror.add_permission(request.clone()).await;
                        Self::emit(
                            mirror,
                            tx,
                            AgentDomainEvent::PermissionPending {
                                asid: request.asid.clone(),
                                request,
                                seq,
                            },
                        );
                    }
                    Err(_) => {
                        let _ = session
                            .client
                            .respond_error(id, -32602, "Invalid permission request")
                            .await;
                    }
                }
            }
            AcpEvent::Disconnected { reason } => {
                session.connected.store(false, Ordering::Release);
                Self::status(
                    session,
                    mirror,
                    tx,
                    AgentSessionStatus::Unknown,
                    Some(AgentErrorInfo {
                        name: "AcpDisconnected".into(),
                        message: format!("{reason}; prompt delivery will not be retried"),
                        status: None,
                        retry_after_ms: None,
                    }),
                )
                .await;
                Self::clear_permissions(session, mirror, tx).await;
                return false;
            }
            _ => {}
        }
        session.connected.load(Ordering::Acquire)
    }

    async fn clear_permissions(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
    ) {
        let mut state = session.state.lock().await;
        state.projection.end_turn();
        let asid = state.info.asid.clone();
        for (request_id, _) in state.permissions.drain() {
            let seq = mirror.resolve_permission(&asid, &request_id).await;
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::PermissionResolved {
                    asid: asid.clone(),
                    request_id,
                    seq,
                },
            );
        }
    }

    async fn finish_usage(
        session: &Session,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        response: Option<&Value>,
    ) {
        let mut state = session.state.lock().await;
        let native_provider = state
            .info
            .model
            .as_ref()
            .map(|model| model.provider_id.as_str())
            .unwrap_or(P::KIND);
        let turn =
            response.and_then(|response| mapper::turn_usage(response, P::KIND, native_provider));
        let total = state.usage.finish(turn);
        if state.info.tokens != total {
            state.info.tokens = total;
            let info = state.info.clone();
            let seq = mirror.update_session(info.clone()).await;
            Self::emit(
                mirror,
                tx,
                AgentDomainEvent::SessionUpdated {
                    asid: info.asid.clone(),
                    info: Box::new(info),
                    seq,
                },
            );
        }
    }

    async fn submit_prompt(
        &self,
        id: &str,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
        native_command: bool,
    ) -> Result<(), AgentError> {
        if delivery.is_some_and(|delivery| delivery != "auto") {
            return Err(AgentError::Unsupported(
                "ACP prompt steering or queueing".into(),
            ));
        }
        let session = self.activate(id, false, true).await?;
        let mut admission = Admission {
            busy: &session.busy,
            handed_off: false,
        };
        let prepared =
            operations::prompt_parts(text, attachments, session.prompt_capabilities).await?;
        let checkpoint = if P::IDLE_RETIREMENT && attachments.is_empty() {
            let state = session.state.lock().await;
            let initial_empty = state.owned_mutations && state.projection.is_empty();
            let directory = state.info.directory.clone().unwrap_or_default();
            drop(state);
            if initial_empty {
                Some(NativeSnapshot::empty(id).checkpoint())
            } else {
                let _inspection = self.inspection.lock().await;
                P::recovery_snapshot(&self.binary, &directory, id, &self.stop)
                    .await
                    .ok()
                    .filter(|snapshot| snapshot.session_id == id)
                    .map(|snapshot| snapshot.checkpoint())
            }
        } else {
            None
        };
        // Admission and the local user row share the state lock: a fast
        // native response cannot appear ahead of its submitted user text.
        // Busy is published only after a request was actually flushed.
        let mut state = session.state.lock().await;
        state.projection.can_submit(&prepared.transcript)?;
        state.recovery = None;
        state.prompt_checkpoint = checkpoint;
        state.prompt_user_digest = Some(recovery::text_digest(&prepared.transcript));
        state.turn_had_output = false;
        state.last_activity = tokio::time::Instant::now();
        // Direct prompt delivery can carry the same native slash command as
        // run_command; catalogs may be stale or omit extension commands.
        if native_command || text.trim_start().starts_with('/') {
            session.client.protect_unverified_live_work();
        }
        let request = session
            .client
            .start_request(
                "session/prompt",
                json!({ "sessionId": id, "prompt": prepared.content }),
            )
            .await;
        match request {
            Ok(request) => {
                state.info.status = AgentSessionStatus::Busy;
                state.info.error = None;
                state.info.updated_ms = mapper::now_ms();
                if let Some(seq) = self
                    .mirror
                    .update_status(&state.info.asid, AgentSessionStatus::Busy, None)
                    .await
                {
                    Self::emit(
                        &self.mirror,
                        &self.tx,
                        AgentDomainEvent::StatusChanged {
                            asid: state.info.asid.clone(),
                            status: AgentSessionStatus::Busy,
                            error: None,
                            seq,
                        },
                    );
                }
                let row = state.projection.submitted(&prepared.transcript);
                let event = self
                    .mirror
                    .record_timeline_upsert(&state.info.asid, vec![row])
                    .await;
                Self::emit(&self.mirror, &self.tx, event);
                drop(state);
                let sender = session.completions.clone();
                tokio::spawn(async move {
                    let _ = sender.send(request.wait().await);
                });
                admission.handed_off = true;
                Ok(())
            }
            Err(error) => {
                drop(state);
                // A partial stdio write can be ambiguous. Preserve the uncertainty;
                // no automatic retry, queue fallback or second delivery is allowed.
                session.connected.store(false, Ordering::Release);
                Self::status(
                    &session,
                    &self.mirror,
                    &self.tx,
                    AgentSessionStatus::Unknown,
                    None,
                )
                .await;
                Err(AgentError::Network(format!(
                    "{error}; prompt delivery may have occurred; do not retry automatically"
                )))
            }
        }
    }

    fn snapshot_matches_projection(snapshot: &NativeSnapshot, projection: &Projection) -> bool {
        if projection.is_empty() {
            return snapshot.entries.is_empty();
        }
        snapshot.last_user.as_deref() == projection.latest_text(TimelineRole::User)
            && snapshot.last_assistant.as_deref().is_some_and(|text| {
                !text.is_empty() && Some(text) == projection.latest_text(TimelineRole::Assistant)
            })
    }

    async fn retire_idle(
        session: &Session,
        context: &ListenerContext,
        events: &mut broadcast::Receiver<AcpEvent>,
        mirror: &MemoryMirror,
        tx: &broadcast::Sender<AgentDomainEvent>,
        stop: &CancellationToken,
    ) -> Option<DormantKind> {
        let Ok(_retirement) = session.retirement.try_lock() else {
            return None;
        };
        if !P::IDLE_RETIREMENT
            || !session.connected.load(Ordering::Acquire)
            || session.client.has_unverified_live_work()
            || session
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        let _admission = Admission {
            busy: &session.busy,
            handed_off: false,
        };
        let (id, directory, evidence) = {
            let state = session.state.lock().await;
            if state.info.status != AgentSessionStatus::Idle
                || !state.permissions.is_empty()
                || !parked_metadata_fits(&state.info)
                || state.last_activity.elapsed()
                    < Duration::from_secs(context.idle_timeout_secs.load(Ordering::Acquire))
            {
                return None;
            }
            let evidence = state.recovery.clone()?;
            (
                state.info.asid.0.clone(),
                state.info.directory.clone()?,
                evidence,
            )
        };
        // At most one potentially large native history inspection per provider.
        let Ok(_inspection) = context.inspection.try_lock() else {
            return None;
        };
        let snapshot = P::recovery_snapshot(&context.binary, &directory, &id, stop)
            .await
            .ok()?;
        let verified = match evidence {
            RecoveryEvidence::Loaded(expected) => {
                snapshot.session_id == id && snapshot.checkpoint() == expected
            }
            RecoveryEvidence::Turn { before, expected } => {
                snapshot.proves_turn(&id, &before, &expected)
            }
        };
        if !verified {
            return None;
        }
        let confirmed = snapshot.checkpoint();
        drop(snapshot);
        let retirement = session.client.retire(RETIREMENT_TIMEOUT);
        tokio::pin!(retirement);
        let mut result = loop {
            tokio::select! {
                biased;
                event = events.recv() => match event {
                    Ok(event) => if !Self::event(session, mirror, tx, event).await {
                        break Err("ACP stream ended during retirement".to_owned());
                    },
                    Err(_) => break Err("ACP retirement stream was interrupted".to_owned()),
                },
                result = &mut retirement => break result,
            }
        };
        if result == Ok(AcpRetirement::NotIdle) {
            return None;
        }
        while let Ok(event) = events.try_recv() {
            if !Self::event(session, mirror, tx, event).await {
                result = Err("ACP retirement stream was interrupted".into());
                break;
            }
        }
        if result.is_ok() {
            result = match P::recovery_snapshot(&context.binary, &directory, &id, stop).await {
                Ok(after) if after.session_id == id && after.checkpoint() == confirmed => {
                    let state = session.state.lock().await;
                    if Self::snapshot_matches_projection(&after, &state.projection)
                        && parked_metadata_fits(&state.info)
                    {
                        Ok(AcpRetirement::Retired)
                    } else {
                        Err("Live history changed during native retirement".into())
                    }
                }
                _ => Err("Native recovery changed during retirement".into()),
            };
        }
        session.connected.store(false, Ordering::Release);
        if result.is_err() {
            Self::status(session, mirror, tx, AgentSessionStatus::Unknown, Some(AgentErrorInfo {
                name: "AcpRetirementUnconfirmed".into(), message: "Native session recovery was not confirmed; explicitly open the conversation before sending; no prompt was retried".into(),
                status: None, retry_after_ms: None,
            })).await;
            return Some(DormantKind::Unexpected);
        }
        let mut state = session.state.lock().await;
        state.info.runtime = Some(parked_runtime());
        let info = state.info.clone();
        let seq = mirror.update_session(info.clone()).await;
        Self::emit(
            mirror,
            tx,
            AgentDomainEvent::SessionUpdated {
                asid: info.asid.clone(),
                info: Box::new(info),
                seq,
            },
        );
        Some(DormantKind::Parked)
    }

    fn listen(
        session: Arc<Session>,
        context: ListenerContext,
        mut events: broadcast::Receiver<AcpEvent>,
        mut completed: mpsc::UnboundedReceiver<Result<Value, String>>,
        mirror: Arc<MemoryMirror>,
        tx: broadcast::Sender<AgentDomainEvent>,
        stop: CancellationToken,
    ) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut dormant_kind = DormantKind::Unexpected;
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    event = events.recv() => match event {
                        Ok(event) => if !Self::event(&session, &mirror, &tx, event).await { break; },
                        Err(_) => { Self::status(&session, &mirror, &tx, AgentSessionStatus::Unknown, None).await; break; }
                    },
                    Some(result) = completed.recv() => {
                        // The RPC reader queued all updates before resolving this reply.
                        while let Ok(event) = events.try_recv() { if !Self::event(&session, &mirror, &tx, event).await { break; } }
                        if !session.connected.load(Ordering::Acquire) { break; }
                        Self::finish_usage(&session, &mirror, &tx, result.as_ref().ok()).await;
                        let normal_end = result.as_ref().is_ok_and(|value| value["stopReason"].as_str() == Some("end_turn"));
                        let (status, error) = match result {
                            Ok(value) => match value["stopReason"].as_str() {
                                Some("cancelled") => (AgentSessionStatus::Interrupted, None),
                                Some("end_turn" | "max_tokens" | "max_turn_requests" | "refusal") => (AgentSessionStatus::Idle, None),
                                _ => (AgentSessionStatus::Unknown, Some(AgentErrorInfo { name: "AcpUnknownStop".into(), message: "ACP returned no recognized stop reason".into(), status: None, retry_after_ms: None })),
                            },
                            Err(error) => (AgentSessionStatus::Failed, Some(AgentErrorInfo { name: "AcpPromptFailed".into(), message: format!("{error}; delivery was not retried"), status: None, retry_after_ms: None })),
                        };
                        Self::clear_permissions(&session, &mirror, &tx).await;
                        Self::status(&session, &mirror, &tx, status, error).await;
                        {
                            let mut state = session.state.lock().await;
                            state.last_activity = tokio::time::Instant::now();
                            state.recovery = if normal_end && state.turn_had_output {
                                match (state.prompt_checkpoint.take(), state.prompt_user_digest.take(), state.projection.latest_text(TimelineRole::Assistant)) {
                                    (Some(before), Some(user_digest), Some(assistant)) if !assistant.is_empty() => Some(RecoveryEvidence::Turn { before, expected: TurnEvidence { user_digest, assistant_digest: recovery::text_digest(assistant) } }),
                                    _ => None,
                                }
                            } else { None };
                        }
                        session.busy.store(false, Ordering::Release);
                    }
                    _ = tick.tick(), if P::IDLE_RETIREMENT => {
                        if let Some(kind) = Self::retire_idle(&session, &context, &mut events, &mirror, &tx, &stop).await {
                            dormant_kind = kind; break;
                        }
                    }
                }
            }
            session.connected.store(false, Ordering::Release);
            Self::clear_permissions(&session, &mirror, &tx).await;
            session.client.shutdown().await;
            let mut state = session.state.lock().await;
            let id = state.info.asid.0.clone();
            if dormant_kind == DormantKind::Unexpected {
                state.info.runtime = Some(SessionRuntime {
                    transcript: Some(TranscriptSource::Snapshot),
                    source: RuntimeSource::Native,
                    interaction: RuntimeInteraction::Unavailable,
                    status: Some(TerminalRuntimeStatus::Unknown),
                    terminal: None,
                    reason: Some(RuntimeReason::NotLoaded),
                });
                let info = state.info.clone();
                let seq = mirror.update_session(info.clone()).await;
                Self::emit(
                    &mirror,
                    &tx,
                    AgentDomainEvent::SessionUpdated {
                        asid: info.asid.clone(),
                        info: Box::new(info),
                        seq,
                    },
                );
            }
            let compact = DormantSession::new(&state.info, state.owned_mutations, dormant_kind);
            drop(state);
            let mut sessions = context.sessions.lock().unwrap();
            if sessions
                .get(&id)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                let mut dormant = context.dormant.lock().unwrap();
                if dormant.len() < MAX_DORMANT_SESSIONS || dormant.contains_key(&id) {
                    dormant.insert(id.clone(), compact);
                    sessions.remove(&id);
                }
            }
            session.finished.cancel();
        });
    }
}

impl<P: AcpProvider> Drop for AcpDriver<P> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl<P: AcpProvider> AgentPort for AcpDriver<P> {
    fn kind(&self) -> &'static str {
        P::KIND
    }
    fn supports_earlier_timeline(&self) -> bool {
        true
    }
    fn load_earlier_timeline<'a>(
        &'a self,
        id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            if limit == 0 {
                return Err(AgentError::InvalidRequest(
                    "history page limit must be positive".into(),
                ));
            }
            // Each accepted ACP load already validates the complete native replay.
            // Older retained rows page through MemoryMirror before this hook runs.
            if self.readonly_empty_snapshot(id).await? {
                return Ok(Vec::new());
            }
            if self.retained(id).is_none() {
                // A terminal-held transcript is mirrored whole; nothing older.
                if self.terminal_live(id).await.is_none() {
                    self.observed_snapshot(id).await?;
                }
                return Ok(Vec::new());
            }

            Ok(Vec::new())
        })
    }
    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async { Ok(self.is_connected()) })
    }
    fn account_limits(
        &self,
    ) -> AgentFuture<'_, crate::agents::domain::account_limits::AccountLimits> {
        P::account_limits(&self.binary, &self.default_directory, &self.stop)
    }
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::NativeUploads
    }
    fn slash_commands_listed(&self) -> bool {
        self.commands_observed.load(Ordering::Acquire)
    }
    fn run_command<'a>(
        &'a self,
        id: &'a str,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self.ensure_loaded(id).await?;
            let prompt = operations::command_prompt(
                &session.state.lock().await.catalog.commands,
                name,
                arguments,
            )?;
            self.submit_prompt(id, &prompt, &[], delivery, true).await
        })
    }
    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let mut directories = HashSet::new();
            Ok(self
                .native_sessions(None)
                .await?
                .into_iter()
                .filter_map(|info| info.directory)
                .filter(|directory| directories.insert(directory.clone()))
                .map(|directory| AgentProject {
                    id: directory.clone(),
                    canonical: directory.clone(),
                    name: std::path::Path::new(&directory)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| directory.clone()),
                    vcs: None,
                    sandboxes: Vec::new(),
                    missing: !std::path::Path::new(&directory).is_dir(),
                })
                .collect())
        })
    }
    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            if query.cursor.is_some() {
                return Err(AgentError::Unsupported("session pagination".into()));
            }
            let mut sessions = self.native_sessions(query.directory.as_deref()).await?;
            sessions.retain(|info| {
                query.keeps_parent(info.parent_id.as_deref())
                    && query.search.as_ref().is_none_or(|search| {
                        info.title.to_lowercase().contains(&search.to_lowercase())
                    })
            });
            sessions.sort_by(|a, b| {
                b.updated_ms
                    .cmp(&a.updated_ms)
                    .then_with(|| a.asid.0.cmp(&b.asid.0))
            });
            if query.order.as_deref() == Some("asc") {
                sessions.reverse();
            }
            if let Some(limit) = query.limit {
                sessions.truncate(limit);
            }
            Ok(sessions)
        })
    }
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if model.is_some() && !P::MODEL_SELECTION {
                return Err(AgentError::Unsupported("ACP model selection".into()));
            }
            if mode.is_some() {
                return Err(AgentError::Unsupported("ACP mode selection".into()));
            }
            let directory = directory.unwrap_or(&self.default_directory);
            if !std::path::Path::new(directory).is_dir() {
                return Err(AgentError::WorkspaceMissing(directory.into()));
            }
            let _mutation = self.mutations.lock().await;
            self.invalidate_session_lists().await;
            let opened = self.open(directory, None).await;
            self.invalidate_session_lists().await;
            let session = opened?;
            if let Some(model) = model {
                if let Err(error) = self.select_model(&session, model).await {
                    let id = session.state.lock().await.info.asid.0.clone();
                    return Err(AgentError::RequestFailed(format!("ACP session {id} was created, but model selection failed: {error}; do not retry creation")));
                }
            }
            let info = session.state.lock().await.info.clone();
            Ok(info)
        })
    }
    fn get_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if let Some(session) = self.retained(id) {
                return Ok(session.state.lock().await.info.clone());
            }
            let dormant = self.dormant.lock().unwrap().get(id).cloned();
            if let Some(entry) = dormant {
                return self.dormant_info(id, &entry).await;
            }
            self.native_session_info(id).await
        })
    }
    fn session_runtime<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Option<SessionRuntime>> {
        Box::pin(async move { Ok(self.terminal_live(id).await.map(|live| live.runtime)) })
    }
    fn prepare_selected_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(live) = self.terminal_live(id).await {
                self.refresh_terminal_snapshot(id, &live).await?;
                self.follow_terminal(id.to_owned());
                return Ok(());
            }
            if self.readonly_empty_snapshot(id).await? {
                return Ok(());
            }
            self.activate(id, true, false).await.map(|_| ())
        })
    }
    fn prepare_observed_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if self.retained(id).is_some() || self.dormant.lock().unwrap().contains_key(id) {
                return Ok(());
            }
            if let Some(live) = self.terminal_live(id).await {
                return self.refresh_terminal_snapshot(id, &live).await;
            }
            if self.readonly_empty_snapshot(id).await? {
                return Ok(());
            }
            // A saved/native identity is metadata, not foreground activation.
            Err(AgentError::NotAvailable(
                "Explicitly open this conversation to load its native history".into(),
            ))
        })
    }
    fn send_prompt<'a>(
        &'a self,
        id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(live) = self.terminal_live(id).await {
                return self.send_to_terminal(id, live, text, attachments).await;
            }
            self.submit_prompt(id, text, attachments, delivery, false)
                .await
        })
    }
    fn interrupt<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(live) = self.terminal_live(id).await {
                let (Some(route), Some(bridge)) = (live.route, self.terminal_bridge()) else {
                    return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
                };
                return bridge.interrupt(&route).await;
            }
            let session = self
                .retained(id)
                .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
            if !session.connected.load(Ordering::Acquire) {
                return Err(AgentError::NotAvailable("ACP connection ended".into()));
            }
            session
                .client
                .notify("session/cancel", json!({"sessionId":id}))
                .await
                .map_err(AgentError::Network)
        })
    }
    fn reply_permission<'a>(
        &'a self,
        id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if message.is_some_and(|message| !message.is_empty()) {
                return Err(AgentError::Unsupported(
                    "ACP permission rejection reasons".into(),
                ));
            }
            let session = self
                .retained(id)
                .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
            let mut state = session.state.lock().await;
            let permission = state.permissions.get(request_id).ok_or_else(|| {
                AgentError::InvalidRequest("ACP permission is no longer pending".into())
            })?;
            let option = permission
                .options
                .iter()
                .filter(|(candidate, _)| *candidate == decision)
                .collect::<Vec<_>>();
            if option.len() != 1 {
                return Err(AgentError::Unsupported(
                    "requested ACP permission option is unavailable or ambiguous".into(),
                ));
            }
            session
                .client
                .respond(
                    permission.rpc_id.clone(),
                    json!({"outcome":{"outcome":"selected","optionId":option[0].1}}),
                )
                .await
                .map_err(AgentError::Network)?;
            state.permissions.remove(request_id);
            let asid = AgentSessionId::from(id);
            let seq = self.mirror.resolve_permission(&asid, request_id).await;
            Self::emit(
                &self.mirror,
                &self.tx,
                AgentDomainEvent::PermissionResolved {
                    asid,
                    request_id: request_id.into(),
                    seq,
                },
            );
            Ok(())
        })
    }
    fn get_pending_permissions<'a>(
        &'a self,
        id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            if self.readonly_empty_snapshot(id).await? {
                return Ok(Vec::new());
            }
            if let Some(snapshot) = self.observed_snapshot(id).await? {
                return Ok(snapshot.permissions);
            }
            let session = self.retained(id).ok_or_else(|| {
                AgentError::NotAvailable("ACP generation changed during observation".into())
            })?;
            let pending = session
                .state
                .lock()
                .await
                .permissions
                .values()
                .map(|permission| permission.request.clone())
                .collect();
            Ok(pending)
        })
    }
    fn get_pending_forms<'a>(&'a self, id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async move {
            self.get_session(id).await?;
            Ok(Vec::new())
        })
    }
    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move { self.available_catalog(directory).await })
    }
    fn get_catalog_for_session<'a>(
        &'a self,
        id: Option<&'a str>,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            if let Some(id) = id {
                if self.readonly_empty_snapshot(id).await? {
                    return Ok(AgentCatalog::default());
                }
                if self.retained(id).is_none() {
                    self.observed_snapshot(id).await?;
                    return self.available_catalog(directory).await;
                }
                let session = self.retained(id).ok_or_else(|| {
                    AgentError::NotAvailable(
                        "ACP generation changed during catalog observation".into(),
                    )
                })?;
                let mut catalog = session.state.lock().await.catalog.clone();
                if !P::MODEL_SELECTION {
                    catalog.models.clear();
                    catalog.defaults.model = None;
                }
                // ACP mode changes persist permission defaults outside the current
                // session, so the existing App must not offer them as local choices.
                catalog.modes.clear();
                catalog.defaults.mode = None;
                Ok(catalog)
            } else {
                self.get_catalog(directory).await
            }
        })
    }
    fn get_timeline<'a>(
        &'a self,
        id: &'a str,
        _limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            if self.readonly_empty_snapshot(id).await? {
                return Ok(Vec::new());
            }
            if let Some(snapshot) = self.observed_snapshot(id).await? {
                return Ok(snapshot.timeline);
            }
            let session = self.retained(id).ok_or_else(|| {
                AgentError::NotAvailable("ACP generation changed during observation".into())
            })?;
            let timeline = session
                .state
                .lock()
                .await
                .projection
                .timeline()
                .into_iter()
                .map(|mut row| {
                    row.seq = 0;
                    row
                })
                .collect();
            Ok(timeline)
        })
    }
    fn fork_session<'a>(
        &'a self,
        id: &'a str,
        message_id: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if let Some(live) = self.terminal_live(id).await {
                return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
            }
            let _mutation = self.mutations.lock().await;
            let source = self.retained(id).ok_or_else(|| {
                AgentError::Unsupported("open a Gateway-owned session before forking".into())
            })?;
            if !source.connected.load(Ordering::Acquire)
                || !operations::supports_operation(&source.native_capabilities, "fork")
            {
                return Err(AgentError::Unsupported("native session fork".into()));
            }
            if source
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(AgentError::NotAvailable("session is busy".into()));
            }
            let _admission = Admission {
                busy: &source.busy,
                handed_off: false,
            };
            let info = source.state.lock().await.info.clone();
            if message_id.is_some() {
                return Err(AgentError::Unsupported(
                    "ACP fork at a message boundary".into(),
                ));
            }
            if source.state.lock().await.projection.rows.is_empty() {
                return Err(AgentError::InvalidRequest(
                    "No conversation to branch".into(),
                ));
            }
            let directory = info
                .directory
                .as_deref()
                .ok_or_else(|| protocol("source session has no directory"))?;
            let mut params = operations::fork_params(id, directory, message_id)?;
            params["mcpServers"] = self.mcp_servers();
            let (client, _events, capabilities) = self.initialized(directory).await?;
            if !operations::supports_operation(&capabilities, "fork") {
                client.shutdown().await;
                return Err(AgentError::Unsupported("native session fork".into()));
            }
            let response = client
                .request_with_timeout("session/fork", params, RPC_TIMEOUT)
                .await;
            self.invalidate_session_lists().await;
            client.shutdown().await;
            let response = response.map_err(AgentError::RequestFailed)?;
            let new_id = operations::forked_session_id(id, &response)?;
            if self.retained(&new_id).is_some()
                || self
                    .mirror
                    .session_info(&AgentSessionId::from(new_id.as_str()))
                    .await
                    .is_some()
            {
                return Err(protocol(
                    "fork returned an existing session identity; do not retry automatically",
                ));
            }
            let cloned=self.open(directory,Some(mapper::session(&new_id,directory,&response))).await.map_err(|error|AgentError::RequestFailed(format!("session {new_id} was forked but could not be loaded: {error}; do not retry fork")))?;
            let mut state = cloned.state.lock().await;
            state.owned_mutations = true;
            Ok(state.info.clone())
        })
    }
    fn delete_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(live) = self.terminal_live(id).await {
                return Err(AgentError::TerminalInteraction(Box::new(live.runtime)));
            }
            let parked_owned =
                self.dormant.lock().unwrap().get(id).is_some_and(|entry| {
                    entry.kind == DormantKind::Parked && entry.owned_mutations
                });
            if parked_owned {
                self.ensure_loaded(id).await?;
            }
            let _mutation = self.mutations.lock().await;
            let session = self.retained(id).ok_or_else(|| {
                AgentError::Unsupported(
                    "deletion requires a session created by this Gateway".into(),
                )
            })?;
            if !session.connected.load(Ordering::Acquire)
                || !session.state.lock().await.owned_mutations
                || !operations::supports_operation(&session.native_capabilities, "delete")
            {
                return Err(AgentError::Unsupported(
                    "deletion requires an idle Gateway-created native session".into(),
                ));
            }
            if session
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return Err(AgentError::NotAvailable("session is busy".into()));
            }
            let _admission = Admission {
                busy: &session.busy,
                handed_off: false,
            };
            let params = operations::delete_params(id)?;
            // Qoder resolves deletion through this connection's bound workspace.
            // An unbound temporary client may acknowledge without deleting it.
            let result = session
                .client
                .request_with_timeout("session/delete", params, RPC_TIMEOUT)
                .await;
            self.invalidate_session_lists().await;
            let value = result.map_err(AgentError::RequestFailed)?;
            if !value.is_object() {
                return Err(protocol(
                    "native deletion outcome is unknown; do not retry automatically",
                ));
            }
            session.connected.store(false, Ordering::Release);
            session.client.shutdown().await;
            tokio::time::timeout(Duration::from_secs(5), session.finished.cancelled())
                .await
                .map_err(|_| {
                    AgentError::NotAvailable("deleted session connection has not stopped".into())
                })?;
            self.sessions.lock().unwrap().remove(id);
            self.dormant.lock().unwrap().remove(id);
            if let Some(metadata) = self.session_metadata.lock().await.as_mut() {
                metadata.sessions.remove(id);
            }
            Ok(())
        })
    }
    fn revert_session<'a>(&'a self, _id: &'a str, _message: &'a str) -> AgentFuture<'a, ()> {
        unsupported("revert")
    }
    fn switch_model<'a>(&'a self, id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let session = self.ensure_loaded(id).await?;
            self.select_model(&session, model).await
        })
    }
    fn switch_mode<'a>(&'a self, _id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        unsupported("mode selection")
    }
    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        unsupported("file search")
    }
    fn reply_form<'a>(
        &'a self,
        _id: &'a str,
        _form: &'a str,
        _answers: Value,
    ) -> AgentFuture<'a, ()> {
        unsupported("interactive forms")
    }
    fn get_vcs_diff<'a>(
        &'a self,
        _id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        unsupported("VCS diff")
    }
}

#[cfg(all(test, unix))]
mod admission_tests {
    use super::*;
    use crate::agents::adapters::fx::tests::integration::Fixture;

    #[tokio::test]
    async fn verified_empty_history_is_readonly_without_starting_a_child() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        let mut empty =
            mapper::session("verified-empty", "", &json!({"title":"Empty conversation"}));
        empty.directory = None;
        empty.status = AgentSessionStatus::Unknown;
        empty.runtime = Some(AcpDriver::<
            crate::agents::adapters::qoder::driver::QoderProvider,
        >::empty_runtime());
        let mut nonempty = empty.clone();
        nonempty.asid = "unknown-nonempty".into();
        nonempty.backend_session_id = "unknown-nonempty".into();
        nonempty.runtime = None;
        *driver.session_metadata.lock().await = Some(SessionMetadata {
            listed_at: tokio::time::Instant::now(),
            sessions: [empty.clone(), nonempty]
                .into_iter()
                .map(|info| (info.asid.0.clone(), info))
                .collect(),
        });
        let before = fixture.methods();
        driver
            .prepare_selected_session("verified-empty")
            .await
            .unwrap();
        assert_eq!(driver.get_session("verified-empty").await.unwrap(), empty);
        assert!(driver
            .get_timeline("verified-empty", 100)
            .await
            .unwrap()
            .is_empty());
        assert!(driver
            .get_pending_permissions("verified-empty")
            .await
            .unwrap()
            .is_empty());
        assert!(driver
            .get_pending_forms("verified-empty")
            .await
            .unwrap()
            .is_empty());
        assert!(driver
            .load_earlier_timeline("verified-empty", 80)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            driver
                .get_catalog_for_session(Some("verified-empty"), None)
                .await
                .unwrap(),
            AgentCatalog::default()
        );
        let snapshot = driver.mirror.get_snapshot(&empty.asid).await.unwrap();
        assert_eq!(snapshot.info.asid, empty.asid);
        assert!(snapshot.info.directory.is_none());
        assert!(
            snapshot.timeline.is_empty()
                && snapshot.permissions.is_empty()
                && snapshot.forms.is_empty()
        );
        assert!(matches!(
            driver
                .send_prompt("verified-empty", "do not send", &[], None)
                .await,
            Err(AgentError::NotAvailable(_))
        ));
        assert!(matches!(
            driver.get_timeline("unknown-nonempty", 100).await,
            Err(AgentError::NotAvailable(_))
        ));
        assert!(driver
            .mirror
            .get_snapshot(&AgentSessionId::from("unknown-nonempty"))
            .await
            .is_none());
        assert_eq!(fixture.methods(), before);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn listed_metadata_avoids_a_second_list_before_history_load() {
        let fixture = Fixture::new().await;
        let original = fixture.qoder_driver();
        let info = original.create_session(None, None, None).await.unwrap();
        original.shutdown().await;
        let resumed = fixture.qoder_driver();
        let before = fixture.methods();
        resumed.native_sessions(None).await.unwrap();
        assert_eq!(
            resumed.get_session(&info.asid.0).await.unwrap().asid,
            info.asid
        );
        resumed
            .prepare_selected_session(&info.asid.0)
            .await
            .unwrap();
        let methods = fixture.methods();
        let recent = &methods[before.len()..];
        assert_eq!(
            recent
                .iter()
                .filter(|method| *method == "session/list")
                .count(),
            1
        );
        assert_eq!(
            recent
                .iter()
                .filter(|method| *method == "session/load")
                .count(),
            1
        );
        assert!(!recent.iter().any(|method| method == "session/prompt"));
        resumed
            .session_metadata
            .lock()
            .await
            .as_mut()
            .unwrap()
            .listed_at = tokio::time::Instant::now() - SESSION_METADATA_TTL;
        resumed.session_lists.lock().await.clear();
        resumed.native_session_info(&info.asid.0).await.unwrap();
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|method| *method == "session/list")
                .count(),
            2
        );
        resumed.shutdown().await;
    }

    #[tokio::test]
    async fn complete_lists_share_native_startup_only_within_the_exact_scope() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        let (a, b, c) = tokio::join!(
            driver.native_sessions(None),
            driver.native_sessions(None),
            driver.native_sessions(None)
        );
        assert!(a.unwrap().is_empty() && b.unwrap().is_empty() && c.unwrap().is_empty());
        assert_eq!(fixture.methods(), ["initialize", "session/list"]);
        let directory = fixture.directory.to_str().unwrap();
        driver.native_sessions(Some(directory)).await.unwrap();
        driver.native_sessions(None).await.unwrap();
        driver.native_sessions(Some(directory)).await.unwrap();
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count(),
            2
        );
        assert_eq!(driver.session_lists.lock().await.len(), 2);
        driver.shutdown().await;
        assert!(driver.native_sessions(None).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_failed_lists_share_a_total_timeout_including_gate_wait() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        std::fs::write(fixture.directory.join("list-delay"), "1").unwrap();
        std::fs::write(fixture.directory.join("fail-list"), "1").unwrap();
        let budget = Duration::from_millis(350);
        let (a, b, c, d) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                driver.native_sessions_with_timeout(None, budget),
                driver.native_sessions_with_timeout(None, budget),
                driver.native_sessions_with_timeout(None, budget),
                driver.native_sessions_with_timeout(None, budget)
            )
        })
        .await
        .unwrap();
        let results = [a, b, c, d];
        assert!(results.iter().all(Result::is_err));
        assert!(results.iter().any(|result| matches!(result,
            Err(AgentError::NotAvailable(message)) if message == "ACP session listing timed out"
        )));
        assert!(driver.session_lists.lock().await.is_empty());
        assert!(driver
            .connections
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.strong_count() == 0));
        // A failed caller cannot make every waiter start another slow refresh.
        assert!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count()
                < 4
        );
        std::fs::remove_file(fixture.directory.join("list-delay")).unwrap();
        std::fs::remove_file(fixture.directory.join("fail-list")).unwrap();
        assert!(driver.native_sessions(None).await.unwrap().is_empty());
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn stopping_cancels_both_list_refresh_and_same_scope_waiters() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        std::fs::write(fixture.directory.join("list-delay"), "1").unwrap();
        std::fs::write(fixture.directory.join("fail-list"), "1").unwrap();
        let listing_driver = driver.clone();
        let listing = tokio::spawn(async move {
            tokio::join!(
                listing_driver.native_sessions(None),
                listing_driver.native_sessions(None),
                listing_driver.native_sessions(None)
            )
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.directory.join("list-started").exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        driver.stop.cancel();
        let (a, b, c) = tokio::time::timeout(Duration::from_secs(1), listing)
            .await
            .unwrap()
            .unwrap();
        for result in [a, b, c] {
            assert!(
                matches!(result, Err(AgentError::NotAvailable(message)) if message == "ACP runtime stopped")
            );
        }
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count(),
            1
        );
        assert!(driver.session_lists.lock().await.is_empty());
        assert!(driver
            .connections
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.strong_count() == 0));
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn unchanged_native_store_keeps_listing_without_a_new_process() {
        static STORE: AtomicU64 = AtomicU64::new(1);
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        driver.set_session_store(|| Some(STORE.load(Ordering::SeqCst)));
        let lists = || {
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count()
        };
        driver.native_sessions(None).await.unwrap();
        // (list age, store changed, native list launched)
        for (by, changed, relisted) in [
            (SESSION_LIST_TTL, false, false),
            (SESSION_LIST_TTL, true, true),
            (SESSION_STORE_MAX_AGE, false, true),
        ] {
            let before = lists();
            driver.session_lists.lock().await[0].listed_at = tokio::time::Instant::now() - by;
            if changed {
                STORE.fetch_add(1, Ordering::SeqCst);
            }
            driver.native_sessions(None).await.unwrap();
            assert_eq!(lists() - before, usize::from(relisted), "{by:?} {changed}");
        }
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn list_expiry_observes_external_sessions_and_failures_are_not_cached() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        assert!(driver.native_sessions(None).await.unwrap().is_empty());
        std::fs::write(
            fixture.directory.join("sessions.json"),
            r#"{"external":[]}"#,
        )
        .unwrap();
        assert!(driver.native_sessions(None).await.unwrap().is_empty());
        driver.session_lists.lock().await[0].listed_at =
            tokio::time::Instant::now() - SESSION_LIST_TTL;
        std::fs::write(fixture.directory.join("fail-list"), "1").unwrap();
        assert!(driver.native_sessions(None).await.is_err());
        assert!(driver.native_sessions(None).await.is_err());
        assert!(driver.session_lists.lock().await.is_empty());
        std::fs::remove_file(fixture.directory.join("fail-list")).unwrap();
        let listed = driver.native_sessions(None).await.unwrap();
        assert_eq!(listed[0].asid.0, "external");
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count(),
            4
        );
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn partial_lists_do_not_become_successful_snapshots_or_negative_identity_lookups() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        driver.native_sessions(None).await.unwrap();
        std::fs::write(
            fixture.directory.join("sessions.json"),
            r#"{"external":[]}"#,
        )
        .unwrap();
        // A young list may omit an external identity, but an exact lookup
        // still asks native metadata rather than declaring the identity gone.
        assert_eq!(
            driver.get_session("external").await.unwrap().asid.0,
            "external"
        );
        driver.invalidate_session_lists().await;
        std::fs::write(fixture.directory.join("fail-list-page"), "1").unwrap();
        assert!(driver.native_sessions(None).await.is_err());
        assert!(driver.native_sessions(None).await.is_err());
        assert!(driver.session_lists.lock().await.is_empty());
        assert!(driver.session_metadata.lock().await.is_none());
        std::fs::remove_file(fixture.directory.join("fail-list-page")).unwrap();
        assert_eq!(driver.native_sessions(None).await.unwrap().len(), 1);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn list_query_limits_and_order_never_truncate_the_cached_native_snapshot() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        std::fs::write(
            fixture.directory.join("sessions.json"),
            r#"{"first":[],"second":[]}"#,
        )
        .unwrap();
        let limited = driver
            .list_sessions(&SessionQuery {
                limit: Some(1),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(limited.len(), 1);
        let all = driver
            .list_sessions(&SessionQuery {
                order: Some("asc".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].asid.0, "second");
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count(),
            1
        );
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn owned_changes_invalidate_lists_and_cached_rows_keep_live_state() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        driver.native_sessions(None).await.unwrap();
        let info = driver.create_session(None, None, None).await.unwrap();
        let listed = driver.native_sessions(None).await.unwrap();
        assert_eq!(listed.len(), 1);
        let session = driver.retained(&info.asid.0).unwrap();
        session.state.lock().await.info.title = "Latest local title".into();
        assert_eq!(
            driver.native_sessions(None).await.unwrap()[0].title,
            "Latest local title"
        );
        assert_eq!(
            fixture
                .methods()
                .iter()
                .filter(|m| *m == "session/list")
                .count(),
            2
        );
        driver
            .switch_model(
                &info.asid.0,
                &ModelRef {
                    provider_id: "qoder".into(),
                    model_id: "other-model".into(),
                    variant: None,
                },
            )
            .await
            .unwrap();
        assert!(driver.session_lists.lock().await.is_empty());
        driver.native_sessions(None).await.unwrap();
        // This executable is the protocol fixture, never a real model process.
        driver
            .send_prompt(&info.asid.0, "fixture turn", &[], None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while session.busy.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let forked = driver.fork_session(&info.asid.0, None).await.unwrap();
        assert!(driver.session_lists.lock().await.is_empty());
        assert_eq!(driver.native_sessions(None).await.unwrap().len(), 2);
        driver.delete_session(&forked.asid.0).await.unwrap();
        assert!(driver.session_lists.lock().await.is_empty());
        let listed = driver.native_sessions(None).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].asid, info.asid);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn a_list_started_before_creation_cannot_repopulate_the_new_revision() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        std::fs::write(fixture.directory.join("list-delay"), "1").unwrap();
        let listing_driver = driver.clone();
        let listing = tokio::spawn(async move { listing_driver.native_sessions(None).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.directory.join("list-started").exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let created = driver.create_session(None, None, None).await.unwrap();
        assert!(listing.await.unwrap().unwrap().is_empty());
        assert!(driver.session_lists.lock().await.is_empty());
        let listed = driver.native_sessions(None).await.unwrap();
        assert_eq!(listed[0].asid, created.asid);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn list_cache_bounds_scopes_and_total_rows_without_retaining_children() {
        let fixture = Fixture::new().await;
        let driver = fixture.qoder_driver();
        let created = driver.create_session(None, None, None).await.unwrap();
        driver.session_lists.lock().await.push(CachedSessionList {
            directory: Some("/older".into()),
            listed_at: tokio::time::Instant::now(),
            fingerprint: None,
            sessions: (0..MAX_CACHED_LIST_ROWS)
                .map(|n| {
                    let mut info = created.clone();
                    info.asid = format!("cached-{n}").into();
                    info
                })
                .collect(),
        });
        driver.native_sessions(None).await.unwrap();
        assert_eq!(driver.session_lists.lock().await.len(), 1);
        for index in 0..MAX_LIST_SCOPES + 2 {
            driver
                .native_sessions(Some(&format!("/scope-{index}")))
                .await
                .unwrap();
        }
        let lists = driver.session_lists.lock().await;
        assert_eq!(lists.len(), MAX_LIST_SCOPES);
        assert!(
            lists.iter().map(|list| list.sessions.len()).sum::<usize>() <= MAX_CACHED_LIST_ROWS
        );
        drop(lists);
        assert_eq!(
            driver
                .connections
                .lock()
                .unwrap()
                .iter()
                .filter(|client| client.strong_count() > 0)
                .count(),
            1,
            "only the explicitly created session retains a native child"
        );
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn unknown_directory_is_not_replaced_with_home_and_native_titles_refresh() {
        let fixture = Fixture::new().await;
        let driver = &fixture.driver;
        let info = driver.create_session(None, None, None).await.unwrap();
        let rows = driver
            .project_listed_sessions(vec![
                json!({
                    "sessionId":info.asid.0, "cwd":"", "title":"Native renamed title"
                }),
                json!({"sessionId":"unknown-directory", "cwd":"", "title":"Unknown"}),
            ])
            .await
            .unwrap();
        assert_eq!(rows[0].directory, info.directory);
        assert_eq!(rows[0].title, "Native renamed title");
        assert!(rows[1].directory.is_none());
        assert_eq!(
            driver
                .mirror
                .get_snapshot(&info.asid)
                .await
                .unwrap()
                .info
                .title,
            "Native renamed title"
        );
        *driver.session_metadata.lock().await = Some(SessionMetadata {
            listed_at: tokio::time::Instant::now(),
            sessions: rows
                .into_iter()
                .map(|info| (info.asid.0.clone(), info))
                .collect(),
        });
        let before = fixture.methods();
        assert!(matches!(
            driver.ensure_loaded("unknown-directory").await,
            Err(AgentError::NotAvailable(_))
        ));
        assert_eq!(fixture.methods(), before);
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn global_catalog_cache_and_failure_preserve_current_commands() {
        let fixture = Fixture::new().await;
        let driver = &fixture.driver;
        let info = driver.create_session(None, None, None).await.unwrap();
        let session = driver.retained(&info.asid.0).unwrap();
        let command = |name: &str| CommandInfo {
            name: name.into(),
            description: None,
            mode: None,
            template: None,
        };
        let mut state = session.state.lock().await;
        let global = state.catalog.clone();
        assert!(!global.models.is_empty());
        assert!(global.commands.is_empty());
        state.catalog.models.clear();
        state.catalog.commands = vec![command("rename")];
        drop(state);
        *driver.global_catalog.lock().await = Some((
            driver.default_directory.clone(),
            tokio::time::Instant::now(),
            Ok(global.clone()),
        ));

        let catalog = driver.available_catalog(None).await.unwrap();
        assert_eq!(catalog.models, global.models);
        assert_eq!(catalog.commands, vec![command("rename")]);
        session.state.lock().await.catalog.commands = vec![command("goal")];
        let catalog = driver.available_catalog(None).await.unwrap();
        assert_eq!(catalog.commands, vec![command("goal")]);
        assert!(driver
            .global_catalog
            .lock()
            .await
            .as_ref()
            .unwrap()
            .2
            .as_ref()
            .unwrap()
            .commands
            .is_empty());

        // This ACP-only fixture returns no CLI metadata. A fresh global lookup
        // fails, and its cached failure must not erase known session commands.
        *driver.global_catalog.lock().await = None;
        for _ in 0..2 {
            let catalog = driver.available_catalog(None).await.unwrap();
            assert!(catalog.models.is_empty());
            assert_eq!(catalog.commands, vec![command("goal")]);
            assert!(driver
                .global_catalog
                .lock()
                .await
                .as_ref()
                .unwrap()
                .2
                .is_err());
        }
        driver.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_before_dispatch_keeps_idle_and_sends_no_prompt() {
        let fixture = Fixture::new().await;
        let info = fixture
            .driver
            .create_session(None, None, None)
            .await
            .unwrap();
        let session = fixture.driver.retained(&info.asid.0).unwrap();
        let projection = session.state.lock().await;
        let driver = fixture.driver.clone();
        let id = info.asid.0.clone();
        let pending =
            tokio::spawn(
                async move { driver.send_prompt(&id, "never dispatched", &[], None).await },
            );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !session.busy.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        assert!(!session.busy.load(Ordering::Acquire));
        assert_eq!(projection.info.status, AgentSessionStatus::Idle);
        drop(projection);
        let snapshot = fixture
            .driver
            .mirror
            .get_snapshot(&info.asid)
            .await
            .unwrap();
        assert_eq!(snapshot.info.status, AgentSessionStatus::Idle);
        assert!(snapshot.timeline.is_empty());
        assert!(!fixture.methods().contains(&"session/prompt".into()));
    }
}

#[cfg(all(test, unix))]
#[path = "idle_tests.rs"]
mod idle_tests;
