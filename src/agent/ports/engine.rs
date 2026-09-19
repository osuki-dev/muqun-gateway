use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::agent::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, FormRequest, ModelRef, PermissionDecision,
    PermissionRequest, SessionQuery, TimelineItem,
};

pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AgentEngineError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum AgentEngineError {
    NotAvailable(String),
    SessionNotFound(String),
    /// A workspace directory the caller named is not on the host any more --
    /// a worktree removed, a throwaway repo deleted, an external drive
    /// unmounted. It carries the path, because the only useful thing to say
    /// about it is which folder went.
    ///
    /// Its own variant because it is the one engine failure that is not a
    /// fault: OpenCode answers a bare 500 for it, and relaying that as a 502
    /// told the user their agent was broken when their folder was simply
    /// gone.
    WorkspaceMissing(String),
    RequestFailed(String),
    Network(String),
    Protocol(String),
}

impl fmt::Display for AgentEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(msg) => write!(f, "Agent engine not available: {msg}"),
            Self::SessionNotFound(id) => write!(f, "Agent session not found: {id}"),
            Self::WorkspaceMissing(path) => {
                write!(f, "The workspace folder is gone: {path}")
            }
            Self::RequestFailed(msg) => write!(f, "Agent request failed: {msg}"),
            Self::Network(msg) => write!(f, "Network error communicating with agent engine: {msg}"),
            Self::Protocol(msg) => write!(f, "Protocol error: {msg}"),
        }
    }
}

impl std::error::Error for AgentEngineError {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDiffItem {
    pub path: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
}

pub trait AgentEnginePort: Send + Sync {
    /// Engine identifier (e.g. "opencode", "claude")
    fn kind(&self) -> &'static str;

    /// Health check / availability probe
    fn probe(&self) -> EngineFuture<'_, bool>;

    /// List known projects / workspaces
    fn list_projects(&self) -> EngineFuture<'_, Vec<AgentProject>>;

    /// List sessions, optionally filtered by directory, parent and search
    fn list_sessions<'a>(&'a self, query: &'a SessionQuery)
        -> EngineFuture<'a, Vec<AgentSessionInfo>>;

    /// Create a new session
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        agent: Option<&'a str>,
    ) -> EngineFuture<'a, AgentSessionInfo>;

    /// Get session metadata
    fn get_session<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, AgentSessionInfo>;

    /// Submit a prompt to a session
    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> EngineFuture<'a, ()>;

    /// Revert session to a previous message and roll back file changes
    fn revert_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> EngineFuture<'a, ()>;

    /// Interrupt current session execution
    fn interrupt<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, ()>;

    /// Switch session active model
    fn switch_model<'a>(
        &'a self,
        session_id: &'a str,
        model: &'a ModelRef,
    ) -> EngineFuture<'a, ()>;

    /// Switch session active agent mode
    fn switch_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
    ) -> EngineFuture<'a, ()>;

    /// Search files in workspace, optionally scoped to a directory
    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>>;

    /// Reply to a permission request, optionally with a rejection reason
    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> EngineFuture<'a, ()>;

    /// Reply to an interactive form
    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> EngineFuture<'a, ()>;

    /// Fetch available catalog (models, agents, MCP)
    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> EngineFuture<'a, AgentCatalog>;

    /// Fetch the VCS diff for a session's directory. `mode` is one of
    /// `working`, `branch` or `committed`; OpenCode requires it.
    fn get_vcs_diff<'a>(
        &'a self,
        session_id: &'a str,
        mode: &'a str,
    ) -> EngineFuture<'a, Vec<FileDiffItem>>;

    /// Permission requests still pending for a session. Used to catch up on
    /// anything raised while the event stream was down -- `/api/event` is
    /// volatile by contract and events during a disconnect are lost.
    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> EngineFuture<'a, Vec<PermissionRequest>>;

    /// Forms still pending for a session, for the same reason.
    fn get_pending_forms<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, Vec<FormRequest>>;

    /// Fetch historical timeline items for a session
    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<TimelineItem>>;
}
