use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::agent::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, FormRequest, ModelRef, PermissionDecision,
    PermissionRequest, SessionQuery, TimelineItem,
};

pub type EngineFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AgentEngineError>> + Send + 'a>>;

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
    Unsupported(String),
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
            Self::Unsupported(cap) => write!(f, "Agent engine does not support capability: {cap}"),
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
    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> EngineFuture<'a, Vec<AgentSessionInfo>>;

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
    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef)
        -> EngineFuture<'a, ()>;

    /// Switch session active agent mode
    fn switch_agent<'a>(&'a self, session_id: &'a str, agent: &'a str) -> EngineFuture<'a, ()>;

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

    /// Delete a session
    fn delete_session<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("delete_session".into())) })
    }

    /// Rename a session
    fn rename_session<'a>(&'a self, _session_id: &'a str, _title: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("rename_session".into())) })
    }

    /// Fork a session
    fn fork_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: Option<&'a str>,
    ) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async { Err(AgentEngineError::Unsupported("fork_session".into())) })
    }

    /// Clear a staged revert
    fn clear_revert<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("clear_revert".into())) })
    }

    /// Stage revert to message_id
    fn stage_revert<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
        _files: Option<bool>,
    ) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("stage_revert".into())) })
    }

    /// Commit revert
    fn commit_revert<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("commit_revert".into())) })
    }

    /// Move session to a new directory
    fn move_session<'a>(
        &'a self,
        _session_id: &'a str,
        _directory: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("move_session".into())) })
    }

    /// Compact a session's history
    fn compact_session<'a>(
        &'a self,
        _session_id: &'a str,
        _delivery: Option<&'a str>,
    ) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("compact_session".into())) })
    }

    /// Get session context
    fn get_context<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentEngineError::Unsupported("get_context".into())) })
    }

    /// Mark session as backgrounded
    fn background_session<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("background_session".into())) })
    }

    /// Wait for session idle
    fn wait_session<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("wait_session".into())) })
    }

    /// Mark session as viewed
    fn view_session<'a>(&'a self, _session_id: &'a str, _idle: u64) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("view_session".into())) })
    }

    /// Export session transcript
    fn export_session<'a>(
        &'a self,
        _session_id: &'a str,
        _sanitize: Option<bool>,
    ) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("export_session".into())) })
    }

    /// Run a slash command in session
    fn run_command<'a>(
        &'a self,
        _session_id: &'a str,
        _name: &'a str,
        _arguments: &'a str,
        _delivery: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("run_command".into())) })
    }

    /// Worktree operations
    fn list_worktrees<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentEngineError::Unsupported("list_worktrees".into())) })
    }

    fn create_worktree<'a>(
        &'a self,
        _directory: Option<&'a str>,
        _input: &'a serde_json::Value,
    ) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("create_worktree".into())) })
    }

    fn remove_worktree<'a>(
        &'a self,
        _directory: Option<&'a str>,
        _worktree: &'a str,
        _force: Option<bool>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("remove_worktree".into())) })
    }

    fn refresh_worktrees<'a>(&'a self, _directory: Option<&'a str>) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("refresh_worktrees".into())) })
    }

    /// Skill operations
    fn get_skills<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentEngineError::Unsupported("get_skills".into())) })
    }

    fn activate_skill<'a>(
        &'a self,
        _session_id: &'a str,
        _name: &'a str,
        _resume: Option<bool>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("activate_skill".into())) })
    }

    /// Shell operations
    fn list_shells<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentEngineError::Unsupported("list_shells".into())) })
    }

    fn get_shell<'a>(&'a self, _shell_id: &'a str) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("get_shell".into())) })
    }

    fn get_shell_output<'a>(
        &'a self,
        _shell_id: &'a str,
        _cursor: Option<u64>,
        _limit: Option<usize>,
    ) -> EngineFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentEngineError::Unsupported("get_shell_output".into())) })
    }

    fn kill_shell<'a>(&'a self, _shell_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("kill_shell".into())) })
    }

    /// Inbox operations
    fn get_inbox<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentEngineError::Unsupported("get_inbox".into())) })
    }

    fn cancel_inbox_item<'a>(
        &'a self,
        _session_id: &'a str,
        _inbox_id: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("cancel_inbox_item".into())) })
    }

    fn set_inbox_delivery<'a>(
        &'a self,
        _session_id: &'a str,
        _inbox_id: &'a str,
        _delivery: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async { Err(AgentEngineError::Unsupported("set_inbox_delivery".into())) })
    }

    /// Saved permissions operations
    fn list_saved_permissions<'a>(
        &'a self,
        _project_id: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async {
            Err(AgentEngineError::Unsupported(
                "list_saved_permissions".into(),
            ))
        })
    }

    fn delete_saved_permission<'a>(&'a self, _id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async {
            Err(AgentEngineError::Unsupported(
                "delete_saved_permission".into(),
            ))
        })
    }
}
