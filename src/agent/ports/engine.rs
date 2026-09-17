use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::agent::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, ModelRef, PermissionDecision, TimelineItem,
};

pub type EngineFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AgentEngineError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum AgentEngineError {
    NotAvailable(String),
    SessionNotFound(String),
    RequestFailed(String),
    Network(String),
    Protocol(String),
}

impl fmt::Display for AgentEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(msg) => write!(f, "Agent engine not available: {msg}"),
            Self::SessionNotFound(id) => write!(f, "Agent session not found: {id}"),
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

    /// List active sessions, optionally filtered by directory
    fn list_sessions<'a>(&'a self, directory: Option<&'a str>) -> EngineFuture<'a, Vec<AgentSessionInfo>>;

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

    /// Search files in workspace
    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<serde_json::Value>>;

    /// Reply to a permission request
    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
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

    /// Fetch VCS diff for current session
    fn get_vcs_diff<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, Vec<FileDiffItem>>;

    /// Fetch historical timeline items for a session
    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<TimelineItem>>;
}
