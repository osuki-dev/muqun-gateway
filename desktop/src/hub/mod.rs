//! `SessionHub`: the one backend abstraction the desktop UI knows
//! (architecture §3).
//!
//! Every argument and result is the gateway's own `agents::domain` type --
//! there is no second session model. P0 implements the subset the OpenCode
//! slice needs; `earlier`, `reply_form`, `catalog`, the switches, `rename`,
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
    AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, PermissionDecision,
    SessionQuery,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use tokio::sync::broadcast;

pub use local::LocalHub;

/// The agent a fresh install suggests first.
pub const OPENCODE: &str = "opencode";

/// Every agent id the gateway knows, in its order of preference
/// (`AgentRuntime`'s `AGENT_PREFERENCE`).
pub use muqun_gateway::agents::runtime::AGENT_PREFERENCE;

pub type HubFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HubError>> + Send + 'a>>;

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
    /// The session's newest history and what it still has pending.
    fn open<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot>;
    fn prompt<'a>(&'a self, asid: &'a AgentSessionId, text: &'a str) -> HubFuture<'a, ()>;
    fn interrupt<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    fn reply_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
        decision: PermissionDecision,
    ) -> HubFuture<'a, ()>;
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
        }
    }
}

impl std::error::Error for HubError {}
