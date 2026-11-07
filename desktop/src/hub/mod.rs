//! `SessionHub`: the one backend abstraction the desktop UI knows
//! (architecture §3).
//!
//! Every argument and result is the gateway's own `agents::domain` type --
//! there is no second session model. [`LocalHub`] runs the gateway's agents
//! plane in this process; a `RemoteHub` over the App's HTTP contract can
//! implement the same trait later without the UI changing.

mod local;
mod remote;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use muqun_gateway::agents::directories::Completion;
use muqun_gateway::agents::ports::{AgentError, AgentSessionSnapshot, FileDiffItem};
use muqun_gateway::agents::{
    AgentCatalog, AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, ModelRef,
    PermissionDecision, SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::platform::StoredUpload;
use tokio::sync::broadcast;

pub use local::LocalHub;
pub use remote::{discover_local_gateway, RemoteHub};

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

/// One computer the `@` popup may name (App `MentionComputer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMention {
    pub server_id: String,
    pub name: String,
    pub reachable: bool,
    /// The computer this composer already talks to.
    pub current: bool,
}

/// One delegated task, as `GET /api/tasks` lists it (the gateway's `TaskView`,
/// the fields the desktop shows).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct TaskRow {
    #[serde(rename = "task_id", alias = "id", default)]
    pub id: String,
    /// `queued`, `dispatching`, `working`, `waiting`, `idle_unconfirmed`,
    /// `done`, `failed` or `cancelled`.
    #[serde(default)]
    pub state: String,
    /// `delegation` or `command`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub goal: String,
    /// What to call the task (never empty from the gateway).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub progress: Option<String>,
    /// `device`, `agent`, `mcp` or `peer`.
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub parent_task_id: Option<String>,
    #[serde(default)]
    pub updated_ms: u64,
    /// The agent the task runs on, from its binding.
    #[serde(skip)]
    pub agent_id: Option<String>,
    /// The agent session the task is bound to, when it is one.
    #[serde(skip)]
    pub asid: Option<String>,
    /// The folder, when the gateway names one (a task view does not today).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub children: TaskChildren,
    /// Whether the user may confirm it is finished (`POST …/complete`):
    /// a device-opened task that is paused.
    #[serde(skip)]
    pub completable: bool,
}

impl TaskRow {
    /// Read one `TaskView` JSON object (a list entry or a create answer).
    pub fn from_json(value: &serde_json::Value) -> Result<Self, serde_json::Error> {
        let mut row: Self = serde_json::from_value(value.clone())?;
        let binding = value.get("binding");
        let text = |key: &str| {
            binding
                .and_then(|b| b.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        row.agent_id = text("agent_id");
        row.asid = text("asid").or_else(|| {
            value
                .get("asid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
        row.completable =
            row.origin == "device" && matches!(row.state.as_str(), "idle_unconfirmed" | "waiting");
        Ok(row)
    }

    /// No further transition: cancel and complete do not apply.
    pub fn is_ended(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }
}

/// One "always allow" decision remembered for a session's project
/// (`PermissionSaved.Info` in snake_case).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct SavedPermission {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub resource: String,
}

/// One background shell (`GET /api/agent-shells`).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct ShellRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub pid: Option<u32>,
}

/// One entry from `GET /api/agent-worktrees` (`Worktree.Directory`).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct WorktreeRow {
    #[serde(default)]
    pub directory: String,
    /// Absent on the project's own root (not removable).
    #[serde(default)]
    pub strategy: Option<String>,
}

impl WorktreeRow {
    pub fn is_root(&self) -> bool {
        self.strategy.as_deref().is_none_or(|s| s.is_empty())
    }
}

/// A task's direct children by state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
pub struct TaskChildren {
    #[serde(default)]
    pub total: u32,
    #[serde(default)]
    pub active: u32,
    #[serde(default)]
    pub waiting: u32,
    #[serde(default)]
    pub failed: u32,
}

/// `GET /api/tasks/defaults`: what a task opened without a folder runs in.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Deserialize)]
pub struct TaskDefaults {
    #[serde(default)]
    pub default_cwd: Option<String>,
}

/// A change to the delegated task list, from a hub that is told of them
/// (the gateway's `task.updated` / `task.deleted` frames on `/api/ws`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskHubEvent {
    Updated(Box<TaskRow>),
    Deleted {
        task_id: String,
    },
    /// Task events were missed (reconnect, or the gateway dropped some):
    /// read the list again.
    Resync,
}

/// Where a hub's sessions run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HubMode {
    /// The gateway service on this computer, over loopback.
    LocalGateway,
    /// The agents plane inside this process; no gateway service.
    #[default]
    Standalone,
}

impl std::fmt::Display for HubMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::LocalGateway => "local gateway",
            Self::Standalone => "standalone",
        })
    }
}

/// Timeline rows newer than a sequence the caller holds. `resync` means the
/// mirror no longer covers that sequence: re-read the snapshot instead.
#[derive(Debug, Clone, Default)]
pub struct TimelineCatchUp {
    pub items: Vec<TimelineItem>,
    pub latest_seq: u64,
    pub resync: bool,
}

pub trait SessionHub: Send + Sync {
    /// Which kind of backend this hub is.
    fn mode(&self) -> HubMode;
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
    /// Retained history only — never wakes an agent. `None` means a cold open.
    fn peek<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Option<AgentSessionSnapshot>>;
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
    /// Whether the session runs in the agent's own bypass mode ("YOLO": the
    /// agent stops asking). `false` for an agent without such a control.
    fn auto_approve<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, bool>;
    /// Switch that bypass mode for this session; the answer is what the
    /// agent reports afterwards. The gateway may refuse
    /// (`allow_remote_auto_approve` off), which is an error here.
    fn set_auto_approve<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        enabled: bool,
    ) -> HubFuture<'a, bool>;
    /// Rename the session in the agent that owns it.
    fn rename<'a>(&'a self, asid: &'a AgentSessionId, title: &'a str) -> HubFuture<'a, ()>;
    /// Delete the session and drop it from the mirror.
    fn delete<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    /// Fork into a new session (adapter may refuse). `message_id` is optional.
    fn fork<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: Option<&'a str>,
    ) -> HubFuture<'a, AgentSessionInfo>;
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
    /// The working-tree changes in the session's folder, as the agent's
    /// `get_vcs_diff` reports them (`working` mode).
    fn vcs_diff<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<FileDiffItem>>;
    /// Mark the session read as of now (the App's `POST …/view`).
    fn view<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    /// Stage a rollback to `message_id` without applying it; `files` also
    /// reverts the working tree on commit.
    fn stage_revert<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: &'a str,
        files: bool,
    ) -> HubFuture<'a, ()>;
    /// Apply the staged rollback.
    fn commit_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    /// Drop the staged rollback.
    fn clear_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    /// Compact the session into a summary (`steer` / `queue`; `None` is the
    /// agent's default).
    fn compact<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()>;
    /// Every session below `asid`, at any depth (sub-agents and theirs),
    /// newest update first. Empty for a session with no children.
    fn descendants<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<AgentSessionInfo>>;
    /// Decisions this session's project remembered as "always allow"
    /// (`GET …/permissions/saved`). Empty when the agent has none or no such
    /// list.
    fn list_saved_permissions<'a>(
        &'a self,
        asid: &'a AgentSessionId,
    ) -> HubFuture<'a, Vec<SavedPermission>>;
    /// Forget one remembered decision (`DELETE …/permissions/saved/{id}`).
    fn forget_saved_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        id: &'a str,
    ) -> HubFuture<'a, ()>;
    /// Background shells for `agent` in `directory` (`GET /api/agent-shells`).
    /// Empty when the agent has none.
    fn list_shells<'a>(
        &'a self,
        agent: &'a str,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, Vec<ShellRow>>;
    /// Stop one background shell (`DELETE /api/agent-shells/{id}`).
    fn kill_shell<'a>(&'a self, agent: &'a str, id: &'a str) -> HubFuture<'a, ()>;
    /// Park the session's current turn in the background
    /// (`POST …/background`).
    fn background_session<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()>;
    /// Worktrees under the project folder (`GET /api/agent-worktrees`).
    fn list_worktrees<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
    ) -> HubFuture<'a, Vec<WorktreeRow>>;
    /// Forget one linked worktree (`DELETE /api/agent-worktrees`).
    fn remove_worktree<'a>(
        &'a self,
        agent: &'a str,
        project: &'a str,
        worktree: &'a str,
        force: bool,
    ) -> HubFuture<'a, ()>;
    /// Rediscover worktrees (`POST /api/agent-worktrees/refresh`).
    fn refresh_worktrees<'a>(&'a self, agent: &'a str, directory: &'a str) -> HubFuture<'a, ()>;
    /// Timeline rows with a sequence above `after`, from the mirror.
    fn timeline_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after: u64,
    ) -> HubFuture<'a, TimelineCatchUp>;
    /// Delegated tasks (`GET /api/tasks`), newest update first. `root` lists
    /// that task and everything below it; `state` is a comma list of states.
    fn list_tasks<'a>(
        &'a self,
        root: Option<&'a str>,
        state: Option<&'a str>,
    ) -> HubFuture<'a, Vec<TaskRow>>;
    /// Delegate `goal` (`POST /api/tasks`). `cwd` and `agent_id` default on
    /// the gateway; `peer_server_id` names another computer to give the
    /// agent context about (`computer_ids`).
    fn create_task<'a>(
        &'a self,
        goal: &'a str,
        agent_id: Option<&'a str>,
        cwd: Option<&'a str>,
        peer_server_id: Option<&'a str>,
    ) -> HubFuture<'a, TaskRow>;
    /// Cancel a task (`POST /api/tasks/{id}/cancel`); idempotent.
    fn cancel_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()>;
    /// The user confirms a paused delegation is finished
    /// (`POST /api/tasks/{id}/complete`).
    fn complete_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()>;
    /// What a task opened without a folder would run in.
    fn task_defaults(&self) -> HubFuture<'_, TaskDefaults>;
    /// Computers `@` may name: this machine first, then linked fleet peers
    /// when the hub has them (App `GET /api/peers`).
    fn list_peers(&self) -> HubFuture<'_, Vec<PeerMention>>;
    /// Every session event of every agent, in the order the runtime saw them.
    fn events(&self) -> broadcast::Receiver<AgentDomainEvent>;
    /// Task changes as they happen, for a hub that is told of them; `None`
    /// when the hub has no task ledger to follow.
    fn task_events(&self) -> Option<broadcast::Receiver<TaskHubEvent>> {
        None
    }
    /// The terminal plane (Herdr / tmux panes) of the gateway this hub talks
    /// to: the gateway's JSON answer, parsed by `model/terminals.rs`. Only a
    /// gateway service has one; the in-process hub answers `Unsupported`.
    fn terminal<'a>(&'a self, call: TerminalCall<'a>) -> HubFuture<'a, serde_json::Value> {
        let _ = call;
        Box::pin(async { Err(HubError::Unsupported("terminals")) })
    }
}

/// One terminal-plane request (the App's `/api/sessions/...` routes). There
/// is deliberately nothing here that focuses, closes or creates a pane.
#[derive(Debug, Clone, Copy)]
pub enum TerminalCall<'a> {
    /// The terminal sessions (Herdr, tmux) the gateway serves.
    Sessions,
    /// One session's workspaces, tabs, panes and agents.
    Snapshot { session: &'a str },
    /// The pane's visible screen, with ANSI styling.
    Screen {
        session: &'a str,
        pane: &'a str,
        lines: u32,
    },
    /// Type `text` into the pane (as a paste).
    SendText {
        session: &'a str,
        pane: &'a str,
        text: &'a str,
    },
    /// Press `keys` (`enter`, `esc`, `ctrl+c`, …) in the pane.
    SendKeys {
        session: &'a str,
        pane: &'a str,
        keys: &'a [String],
    },
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
    /// This hub has no such plane (the in-process hub has no terminals).
    Unsupported(&'static str),
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
            Self::Unsupported(what) => write!(f, "{what} need the Muqun gateway service"),
        }
    }
}

impl std::error::Error for HubError {}
