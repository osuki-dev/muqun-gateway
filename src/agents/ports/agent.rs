use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, FormRequest, ModelRef, PermissionDecision,
    PermissionRequest, SessionPage, SessionQuery, TimelineItem,
};

pub type AgentFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AgentError>> + Send + 'a>>;

#[derive(Debug, Clone)]
pub enum AgentError {
    NotAvailable(String),
    SessionNotFound(String),
    /// A workspace directory the caller named is not on the host any more --
    /// a worktree removed, a throwaway repo deleted, an external drive
    /// unmounted. It carries the path, because the only useful thing to say
    /// about it is which folder went.
    ///
    /// Its own variant because it is the one agent failure that is not a
    /// fault: OpenCode answers a bare 500 for it, and relaying that as a 502
    /// told the user their agent was broken when their folder was simply
    /// gone.
    WorkspaceMissing(String),
    RequestFailed(String),
    Network(String),
    Protocol(String),
    Unsupported(String),
    /// The caller asked for something the gateway will not do on its behalf,
    /// such as handing the agent a file it never uploaded. A `400`, not an
    /// agent fault.
    InvalidRequest(String),
    /// A live external runtime must be controlled through its existing terminal.
    TerminalInteraction(Box<crate::agents::domain::SessionRuntime>),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(msg) => write!(f, "Agent not available: {msg}"),
            Self::SessionNotFound(id) => write!(f, "Agent session not found: {id}"),
            Self::WorkspaceMissing(path) => {
                write!(f, "The workspace folder is gone: {path}")
            }
            Self::RequestFailed(msg) => write!(f, "Agent request failed: {msg}"),
            Self::Network(msg) => write!(f, "Network error communicating with the agent: {msg}"),
            Self::Protocol(msg) => write!(f, "Protocol error: {msg}"),
            Self::Unsupported(cap) => write!(f, "Agent does not support capability: {cap}"),
            Self::InvalidRequest(msg) => write!(f, "Invalid request: {msg}"),
            Self::TerminalInteraction(_) => write!(
                f,
                "Native interaction is unavailable; use the current terminal binding when provided"
            ),
        }
    }
}

impl std::error::Error for AgentError {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileDiffItem {
    pub path: String,
    pub patch: String,
    pub additions: usize,
    pub deletions: usize,
}

/// How a driver takes the files attached to a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentMode {
    /// The agent's own API carries file parts.
    Native,
    /// Native file parts, restricted to files uploaded to this Gateway.
    /// Remote URLs, data URIs and arbitrary host paths are not accepted.
    NativeUploads,
    /// The agent has no attachment API but can read files off this host, so
    /// the prompt use case lists the paths in the text instead.
    ByPath,
}

/// A new service-owned session that an official terminal client can attach to.
/// No prompt has been submitted; the caller performs its existing one-shot delivery.
pub struct PreparedTerminalLaunch {
    pub session: AgentSessionInfo,
    pub arguments: Vec<String>,
}

/// `text` followed by a block listing `paths` for an agent that reads files
/// itself. Unchanged when there are no paths.
pub fn append_attachment_paths(text: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        return text.to_string();
    }
    let mut out = format!("{text}\n\nAttached files (on this host):");
    for path in paths {
        out.push_str("\n- ");
        out.push_str(path);
    }
    out
}

pub trait AgentPort: Send + Sync {
    /// Agent identifier (e.g. "opencode", "claude")
    fn kind(&self) -> &'static str;

    /// Health check / availability probe
    fn probe(&self) -> AgentFuture<'_, bool>;

    /// List known projects / workspaces
    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>>;

    /// List sessions, optionally filtered by directory, parent and search
    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>>;

    /// Read one ordered page from a single provider, without losing cursors.
    fn list_sessions_page<'a>(&'a self, _query: &'a SessionQuery) -> AgentFuture<'a, SessionPage> {
        Box::pin(async { Err(AgentError::Unsupported("session pagination".into())) })
    }

    /// Create a new session
    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo>;

    /// Prepare a shared session from supported native CLI choices. Unsupported
    /// is returned only before creating a session; other failures are ambiguous
    /// and must never trigger a second launch or prompt delivery.
    fn prepare_terminal_launch<'a>(
        &'a self,
        _directory: &'a str,
        _title: &'a str,
        _arguments: &'a [String],
    ) -> AgentFuture<'a, PreparedTerminalLaunch> {
        Box::pin(async { Err(AgentError::Unsupported("shared terminal startup".into())) })
    }

    /// Get session metadata
    fn get_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, AgentSessionInfo>;

    /// Prepare a user-selected snapshot, even when its retained mirror is warm.
    /// Must not replace history or submit a prompt. Discovery and metadata-only
    /// reads do not call this hook. Rejected ownership must remain an error.
    fn prepare_selected_session<'a>(&'a self, _id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Optional fresh runtime observation, without reading/replacing transcript history.
    fn session_runtime<'a>(
        &'a self,
        _id: &'a str,
    ) -> AgentFuture<'a, Option<crate::agents::domain::SessionRuntime>> {
        Box::pin(async { Ok(None) })
    }

    /// Whether `send_prompt` takes attachments natively or wants them listed
    /// in the text as host paths.
    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::Native
    }

    /// Submit a prompt to a session
    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()>;

    /// Whether the agent's service takes an MCP server at runtime
    /// ([`AgentPort::add_mcp_server`]).
    fn adds_mcp_servers(&self) -> bool {
        false
    }

    /// Whether the MCP server `name` is connected for `directory`.
    fn mcp_server_connected<'a>(
        &'a self,
        _name: &'a str,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    /// Register (or replace) the MCP server `name` with `config` for
    /// `directory` until the service restarts; no configuration file is
    /// written.
    fn add_mcp_server<'a>(
        &'a self,
        _name: &'a str,
        _config: &'a serde_json::Value,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async {
            Err(AgentError::InvalidRequest(
                "this agent takes no MCP server at runtime".into(),
            ))
        })
    }

    /// Attach (or replace) a durable instruction `text` under `key` to
    /// session `session_id`, outside its conversation. `Ok(false)` when the
    /// agent has no such thing.
    fn set_session_instruction<'a>(
        &'a self,
        _session_id: &'a str,
        _key: &'a str,
        _text: &'a str,
    ) -> AgentFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    /// Remove the instruction under `key` from session `session_id`.
    fn remove_session_instruction<'a>(
        &'a self,
        _session_id: &'a str,
        _key: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    /// Revert session to a previous message and roll back file changes
    fn revert_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> AgentFuture<'a, ()>;

    /// Interrupt current session execution
    fn interrupt<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()>;

    /// Switch session active model
    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()>;

    /// Switch session active agent mode
    fn switch_mode<'a>(&'a self, session_id: &'a str, mode: &'a str) -> AgentFuture<'a, ()>;

    /// Search files in workspace, optionally scoped to a directory
    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<serde_json::Value>>;

    /// Reply to a permission request, optionally with a rejection reason
    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()>;

    /// Reply to an interactive form
    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> AgentFuture<'a, ()>;

    /// Fetch available catalog (models, agents, MCP)
    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog>;

    /// Fetch the VCS diff for a session's directory. `mode` is one of
    /// `working`, `branch` or `committed`; OpenCode requires it.
    fn get_vcs_diff<'a>(
        &'a self,
        session_id: &'a str,
        mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>>;

    /// Permission requests still pending for a session. Used to catch up on
    /// anything raised while the event stream was down -- `/api/event` is
    /// volatile by contract and events during a disconnect are lost.
    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>>;

    /// Forms still pending for a session, for the same reason.
    fn get_pending_forms<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, Vec<FormRequest>>;

    /// Fetch historical timeline items for a session
    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>>;

    /// Delete a session
    fn delete_session<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("delete_session".into())) })
    }

    /// Rename a session
    fn rename_session<'a>(&'a self, _session_id: &'a str, _title: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("rename_session".into())) })
    }

    /// Fork a session
    fn fork_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async { Err(AgentError::Unsupported("fork_session".into())) })
    }

    /// Clear a staged revert
    fn clear_revert<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("clear_revert".into())) })
    }

    /// Stage revert to message_id
    fn stage_revert<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
        _files: Option<bool>,
    ) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("stage_revert".into())) })
    }

    /// Commit revert
    fn commit_revert<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("commit_revert".into())) })
    }

    /// Move session to a new directory
    fn move_session<'a>(
        &'a self,
        _session_id: &'a str,
        _directory: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("move_session".into())) })
    }

    /// Compact a session's history
    fn compact_session<'a>(
        &'a self,
        _session_id: &'a str,
        _delivery: Option<&'a str>,
    ) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("compact_session".into())) })
    }

    /// Get session context
    fn get_context<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("get_context".into())) })
    }

    /// Mark session as backgrounded
    fn background_session<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("background_session".into())) })
    }

    /// Wait for session idle
    fn wait_session<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("wait_session".into())) })
    }

    /// Mark session as viewed
    fn view_session<'a>(&'a self, _session_id: &'a str, _idle: u64) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("view_session".into())) })
    }

    /// Export session transcript
    fn export_session<'a>(
        &'a self,
        _session_id: &'a str,
        _sanitize: Option<bool>,
    ) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("export_session".into())) })
    }

    /// Run a slash command in session
    fn run_command<'a>(
        &'a self,
        _session_id: &'a str,
        _name: &'a str,
        _arguments: &'a str,
        _delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("run_command".into())) })
    }

    /// Worktree operations
    fn list_worktrees<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("list_worktrees".into())) })
    }

    fn create_worktree<'a>(
        &'a self,
        _directory: Option<&'a str>,
        _input: &'a serde_json::Value,
    ) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("create_worktree".into())) })
    }

    fn remove_worktree<'a>(
        &'a self,
        _directory: Option<&'a str>,
        _worktree: &'a str,
        _force: Option<bool>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("remove_worktree".into())) })
    }

    fn refresh_worktrees<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("refresh_worktrees".into())) })
    }

    /// Skill operations
    fn get_skills<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("get_skills".into())) })
    }

    fn activate_skill<'a>(
        &'a self,
        _session_id: &'a str,
        _name: &'a str,
        _resume: Option<bool>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("activate_skill".into())) })
    }

    /// Shell operations
    fn list_shells<'a>(
        &'a self,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("list_shells".into())) })
    }

    fn get_shell<'a>(&'a self, _shell_id: &'a str) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("get_shell".into())) })
    }

    fn get_shell_output<'a>(
        &'a self,
        _shell_id: &'a str,
        _cursor: Option<u64>,
        _limit: Option<usize>,
    ) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("get_shell_output".into())) })
    }

    fn kill_shell<'a>(&'a self, _shell_id: &'a str) -> AgentFuture<'a, serde_json::Value> {
        Box::pin(async { Err(AgentError::Unsupported("kill_shell".into())) })
    }

    /// Inbox operations
    fn get_inbox<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("get_inbox".into())) })
    }

    fn cancel_inbox_item<'a>(
        &'a self,
        _session_id: &'a str,
        _inbox_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("cancel_inbox_item".into())) })
    }

    fn set_inbox_delivery<'a>(
        &'a self,
        _session_id: &'a str,
        _inbox_id: &'a str,
        _delivery: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("set_inbox_delivery".into())) })
    }

    /// Saved permissions operations
    fn list_saved_permissions<'a>(
        &'a self,
        _project_id: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async { Err(AgentError::Unsupported("list_saved_permissions".into())) })
    }

    fn delete_saved_permission<'a>(&'a self, _id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("delete_saved_permission".into())) })
    }
}
