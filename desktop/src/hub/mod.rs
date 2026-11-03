//! `SessionHub`: the one backend abstraction the desktop UI knows
//! (architecture §3).
//!
//! Every argument and result is the gateway's own `agents::domain` type --
//! there is no second session model. P0 implements the subset the OpenCode
//! slice needs; `catalog`, the switches, `rename`,
//! `delete` and `fork` join in P1. [`LocalHub`] runs the gateway's agents
//! plane in this process; a `RemoteHub` over the App's HTTP contract can
//! implement the same trait later without the UI changing.

mod local;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use muqun_gateway::agents::directories::Completion;
use muqun_gateway::agents::ports::{AgentError, AgentSessionSnapshot};
use muqun_gateway::agents::{
    AgentCatalog, AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, ModelRef,
    PermissionDecision, SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::platform::StoredUpload;
use tokio::sync::broadcast;

pub use local::LocalHub;

/// The agent a fresh install suggests first.
pub const OPENCODE: &str = "opencode";

/// Every agent id the gateway knows, in its order of preference
/// (`AgentRuntime`'s `AGENT_PREFERENCE`).
pub use muqun_gateway::agents::runtime::AGENT_PREFERENCE;

pub type HubFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HubError>> + Send + 'a>>;

/// What the composer offers for one agent in one folder: the gateway's
/// catalogue (models, modes, slash commands, skills).
#[derive(Debug, Clone, Default)]
pub struct CatalogView {
    pub catalog: AgentCatalog,
}

pub trait SessionHub: Send + Sync {
    /// Every agent this hub knows, with the gateway's availability words.
    /// Never waits on a probe: the answer is what is known now (attached
    /// agents, the last probe), and asking again later picks up the result
    /// of the probe this started.
    fn agents(&self) -> HubFuture<'_, Vec<AgentDiscoveryInfo>>;
    /// The ids of the agents attached right now, most preferred first.
    fn attached(&self) -> HubFuture<'_, Vec<String>>;
    /// One agent's sessions, as its adapter lists them.
    fn sessions<'a>(
        &'a self,
        agent: &'a str,
        query: SessionQuery,
    ) -> HubFuture<'a, Vec<AgentSessionInfo>>;
    /// The folders an agent already knows as projects.
    fn projects<'a>(&'a self, agent: &'a str) -> HubFuture<'a, Vec<AgentProject>>;
    /// Folders under a typed prefix, on the host the sessions run on.
    fn directories<'a>(&'a self, prefix: &'a str) -> HubFuture<'a, Completion>;
    fn create<'a>(&'a self, agent: &'a str, cwd: &'a Path) -> HubFuture<'a, AgentSessionInfo>;
    /// The session's newest history and what it still has pending, after
    /// preparing it as the App's `/prepare` does (which may restore a dormant
    /// runtime or start following a terminal). For an explicit selection.
    fn open<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot>;
    /// The same snapshot, read without preparing anything where the hub can
    /// (the gateway's mirror): for re-reading a session that is already open.
    fn snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot>;
    /// The next older page of the session's history, kept in the mirror;
    /// empty when there is nothing older.
    fn earlier<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<TimelineItem>>;
    fn prompt<'a>(&'a self, asid: &'a AgentSessionId, text: &'a str) -> HubFuture<'a, ()>;
    fn interrupt<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    fn reply_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
        decision: PermissionDecision,
    ) -> HubFuture<'a, ()>;
    fn reply_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> HubFuture<'a, ()>;
    /// The prompt with files the hub already stored ([`SessionHub::store_attachment`])
    /// and the delivery the owner asked for (`steer` / `queue`; `None` is the
    /// agent's own default). One call for both, as the App's send.
    fn send<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()>;
    /// Files under `directory` matching `query`, as the agent finds them
    /// (`@` mentions). Each hit is the gateway's `{path, type}` object.
    fn find_files<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
        query: &'a str,
        limit: usize,
    ) -> HubFuture<'a, Vec<serde_json::Value>>;
    /// Models, modes, skills and slash commands for the agent in `directory`
    /// (or for the exact session when one is given).
    fn catalog<'a>(
        &'a self,
        agent: &'a str,
        asid: Option<&'a AgentSessionId>,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, CatalogView>;
    /// Run a host slash command in the session (never sent as typed text).
    fn run_command<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, serde_json::Value>;
    /// Activate a catalogue skill in the session.
    fn activate_skill<'a>(&'a self, asid: &'a AgentSessionId, skill: &'a str) -> HubFuture<'a, ()>;
    fn switch_model<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        model: &'a ModelRef,
    ) -> HubFuture<'a, ()>;
    fn switch_mode<'a>(&'a self, asid: &'a AgentSessionId, mode: &'a str) -> HubFuture<'a, ()>;
    /// A queued or steered prompt: take it back, or move it between the two
    /// deliveries (`steer` runs at the next step boundary).
    fn inbox_cancel<'a>(&'a self, asid: &'a AgentSessionId, item: &'a str) -> HubFuture<'a, ()>;
    fn inbox_delivery<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        item: &'a str,
        delivery: &'a str,
    ) -> HubFuture<'a, ()>;
    /// Store a file the owner attached, under the gateway's own upload rules
    /// (type sniffed from the bytes, 25 MiB / 64 MiB for video, no
    /// executables). The result's path is what [`SessionHub::send`] takes.
    fn store_attachment<'a>(&'a self, name: &'a str, bytes: Vec<u8>)
        -> HubFuture<'a, StoredUpload>;
    /// Every session event of every agent, in the order the runtime saw them.
    fn events(&self) -> broadcast::Receiver<AgentDomainEvent>;
}

/// Why a hub call did not happen.
#[derive(Debug, Clone)]
pub enum HubError {
    /// The agent is not attached: not installed, not running, disabled.
    /// `reason` is the gateway's own explanation when discovery has one.
    AgentUnavailable {
        agent: String,
        reason: Option<String>,
    },
    /// No attached agent owns the session.
    SessionNotFound(String),
    /// The agent refused or failed; the gateway's error, for display.
    Agent(AgentError),
    /// A file was not accepted as an attachment (the gateway's own words).
    Upload(muqun_gateway::platform::UploadRefusal),
}

impl From<AgentError> for HubError {
    fn from(err: AgentError) -> Self {
        Self::Agent(err)
    }
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AgentUnavailable { agent, reason } => {
                write!(f, "{agent} is not reachable")?;
                match reason {
                    Some(reason) => write!(f, ": {reason}"),
                    None => Ok(()),
                }
            }
            Self::SessionNotFound(asid) => write!(f, "Agent session not found: {asid}"),
            Self::Agent(err) => err.fmt(f),
            Self::Upload(refusal) => f.write_str(&refusal.message()),
        }
    }
}

impl std::error::Error for HubError {}
