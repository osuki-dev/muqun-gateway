//! UI → core. The UI thread only ever sends one of these; it never awaits,
//! never takes a runtime lock, never calls the hub (architecture §2).

use std::path::PathBuf;

use muqun_gateway::agents::{AgentSessionId, PermissionDecision};
use tokio::sync::mpsc;

use crate::config::{ConfigEdit, Scheme};

#[derive(Debug, Clone)]
pub enum Command {
    /// Re-read agent status and the session list.
    Refresh,
    /// Make a session the active one and read its snapshot.
    OpenSession(AgentSessionId),
    /// Read a session's snapshot for a read-only look (a subagent's detail
    /// sheet): never prepares it, never makes it the active one.
    ReadDetail(AgentSessionId),
    /// An agent's account rate windows (`Update::AccountLimits`).
    LoadAccountLimits {
        agent: String,
    },
    /// Open a session into a split tile: read its snapshot and stream its
    /// events, without taking the active (focused) place.
    OpenTile(AgentSessionId),
    /// The sessions on screen besides the active one (split tiles). Their
    /// events stream like the active session's; the rest are filtered.
    WatchSessions(Vec<AgentSessionId>),
    /// Start a session for `agent` in `cwd`, remember `cwd`, open it. (The
    /// composer starts sessions through `Compose`; the QA harness uses this.)
    #[allow(dead_code)]
    CreateSession {
        agent: String,
        cwd: PathBuf,
    },
    #[allow(dead_code)]
    SendPrompt {
        asid: AgentSessionId,
        text: String,
    },
    Interrupt(AgentSessionId),
    ReplyPermission {
        asid: AgentSessionId,
        id: String,
        decision: PermissionDecision,
    },
    /// Show the session's changes (one file's, or all) in the diff panel.
    OpenDiff {
        asid: AgentSessionId,
        file: Option<String>,
    },
    /// Stage a rollback to `message_id`, to be confirmed or cancelled
    /// (§6.1c item 9).
    StageRevert {
        asid: AgentSessionId,
        message_id: String,
    },
    /// Apply the staged rollback.
    CommitRevert {
        asid: AgentSessionId,
    },
    /// Drop the staged rollback.
    ClearRevert {
        asid: AgentSessionId,
    },
    /// Compact the session into a summary.
    CompactSession {
        asid: AgentSessionId,
    },
    /// The next older page of the session's history (architecture §3
    /// `earlier`).
    LoadEarlier(AgentSessionId),
    ReplyForm {
        asid: AgentSessionId,
        id: String,
        /// `{ field key: value }`, as the gateway's form route takes it.
        answers: serde_json::Value,
    },
    /// Folders under a typed prefix. Debounced on the core side; only the
    /// answer for the newest prefix is delivered. (The start page picks
    /// folders with the system dialog; this waits for a typed path field.)
    #[allow(dead_code)]
    CompleteDirectory {
        prefix: String,
    },
    /// The folders `agent` already knows as projects.
    LoadProjects {
        agent: String,
    },
    /// The owner changed a setting or arranged the window. Applied at once
    /// in memory, written to disk after 500 ms without another edit.
    Edit(ConfigEdit),
    /// The composer's work (`model/compose.rs`): files, catalogue, staging,
    /// delivery, the queue.
    Compose(super::compose::ComposeCommand),
    /// Themes (`theme/service.rs`). The library list, and the gallery index
    /// (from its 1 h cache when fresh) with its card pictures.
    ThemeRefresh,
    /// Download theme `id` from the gallery, check its SHA-256, install it.
    ThemeInstall(String),
    /// Remove an installed theme (built-ins cannot be).
    ThemeRemove(String),
    /// Make theme `id` current in `scheme`, and remember both.
    ThemeApply(String, Scheme),
    /// Install a `.muqun-theme` from disk (file dialog or drop).
    ThemeImportFile(PathBuf),
    /// Download and install a public HTTPS `.muqun-theme` link (App link import).
    ThemeImportUrl(String),
    /// Rename a listed session.
    RenameSession {
        asid: AgentSessionId,
        title: String,
    },
    /// Delete a listed session (and clear it if it is open).
    DeleteSession(AgentSessionId),
    /// Fork a session into a new one and open it.
    ForkSession {
        asid: AgentSessionId,
        message_id: Option<String>,
    },
    /// Read the sessions below `asid` (its sub-agents, at any depth).
    LoadDescendants {
        asid: AgentSessionId,
    },
    /// Read the project's remembered "always allow" decisions.
    LoadSavedPermissions {
        asid: AgentSessionId,
    },
    /// Forget one remembered decision.
    ForgetSavedPermission {
        asid: AgentSessionId,
        id: String,
    },
    /// Re-read background shells for the open session's agent/folder.
    LoadShells {
        agent: String,
        directory: Option<String>,
    },
    /// Stop one background shell.
    KillShell {
        agent: String,
        id: String,
        directory: Option<String>,
    },
    /// Park the open session's current turn in the background.
    BackgroundSession {
        asid: AgentSessionId,
    },
    /// Re-read worktrees for the open session's project folder.
    LoadWorktrees {
        agent: String,
        directory: String,
    },
    /// Remove a linked worktree.
    RemoveWorktree {
        agent: String,
        project: String,
        worktree: String,
    },
    /// Rediscover worktrees for a project folder.
    RefreshWorktrees {
        agent: String,
        directory: String,
    },
    /// Re-read the delegated task list (and the default folder).
    RefreshTasks,
    /// Delegate `goal` as a task (`agent` / `cwd` default on the gateway).
    CreateTask {
        goal: String,
        agent: Option<String>,
        cwd: Option<String>,
    },
    /// Cancel a task.
    CancelTask {
        id: String,
    },
    /// Confirm a paused delegation is finished.
    CompleteTask {
        id: String,
    },
}

pub type CommandSender = mpsc::UnboundedSender<Command>;
pub type CommandReceiver = mpsc::UnboundedReceiver<Command>;

pub fn channel() -> (CommandSender, CommandReceiver) {
    mpsc::unbounded_channel()
}

/// Send, from the UI thread. The receiver only goes away when the core
/// thread has ended, at which point there is nothing left to tell.
pub fn send(tx: &CommandSender, command: Command) {
    if tx.send(command).is_err() {
        tracing::warn!("the core thread is gone; a UI command was dropped");
    }
}
