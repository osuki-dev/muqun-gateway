//! The task use cases: delegate a goal to an agent session, wait for it,
//! read it, cancel it, and run commands through `run_command`.
//!
//! Every change the service records -- its own and `run_command`'s, which
//! write through [`Publishing`] -- is broadcast as a [`TaskDomainEvent`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use super::domain::{
    clip, now_ms, one_line, ChildCounts, NoticeTarget, PrincipalKind, Requested, Task, TaskBinding,
    TaskChange, TaskEvent, TaskFilter, TaskId, TaskKind, TaskOrigin, TaskPrincipal, TaskState,
    REPORTED_STATES,
};
use super::events::TaskDomainEvent;
use super::pane_agent;
use super::ports::{TaskFuture, TaskStore, TaskStoreError};
use super::run_command::{self, RunHandle, RunRequest};
use crate::agents::domain::{
    AgentDomainEvent, AgentErrorInfo, AgentPart, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, ModelInfo, ModelRef, TimelineItem, TimelineRole, ToolCall,
};
use crate::agents::manager::AgentManager;
use crate::agents::ports::agent::AgentError;
use crate::agents::ports::mirror::SessionMirrorPort;
use crate::agents::runtime::AGENT_PREFERENCE;
use crate::backend::{PaneId, TabId};
use crate::fleet::manifest::is_self;
use crate::fleet::peers::PeerPresence;
use crate::AppState;

/// The longest a `wait` blocks, and the shortest.
const MAX_WAIT: Duration = Duration::from_secs(45);
const MIN_WAIT: Duration = Duration::from_secs(1);
/// The longest `agent_id`, `mode`, `model` or `variant` accepted.
pub(crate) const MAX_NAME_BYTES: usize = 256;
/// Whether `name` is usable as an `agent_id`, `mode`, `model` or `variant`.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_NAME_BYTES
}
/// The largest goal a delegation takes.
const MAX_GOAL_BYTES: usize = 32 * 1024;
/// Whether `goal` is usable: not blank, at most [`MAX_GOAL_BYTES`].
pub(crate) fn valid_goal(goal: &str) -> bool {
    !goal.trim().is_empty() && goal.len() <= MAX_GOAL_BYTES
}

/// Why a task's working directory was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadDirectory {
    NotAbsolute,
    Missing,
    NotADirectory,
}

/// `dir` as a task's working directory: absolute, existing, a directory,
/// canonicalized. There is deliberately no other fence: the caller is the
/// same user.
pub(crate) fn task_directory(dir: &str) -> Result<PathBuf, BadDirectory> {
    let path = Path::new(dir);
    if !path.is_absolute() {
        return Err(BadDirectory::NotAbsolute);
    }
    let canonical = path.canonicalize().map_err(|_| BadDirectory::Missing)?;
    canonical
        .is_dir()
        .then_some(canonical)
        .ok_or(BadDirectory::NotADirectory)
}

/// Timeline items read for a summary.
pub(crate) const SUMMARY_ITEMS: usize = 50;
/// A delegation's `summary`: its result as one line of at most this many
/// characters.
pub(crate) const SUMMARY_CHARS: usize = 240;
/// Timeline items read from the agent for a timeline page, and the page size.
const TIMELINE_READ: usize = 200;
const TIMELINE_PAGE: usize = 50;
/// A timeline page's items stop at about this many bytes of JSON, newest
/// kept, so the page fits an MCP result whole.
const TIMELINE_PAGE_BYTES: usize = 12 * 1024;
const SOURCE: &str = "task_service";
/// The largest completion summary taken (`tasks.complete`, `/complete`),
/// before it is cut to one line of [`SUMMARY_CHARS`].
pub(crate) const MAX_SUMMARY_BYTES: usize = 32 * 1024;
/// How long an ended task (done, failed, cancelled) and its events are kept:
/// long enough to look back on, short of an ever-growing ledger (an
/// `exec.run` summary alone holds up to 8 KiB of output).
pub(crate) const TASK_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// [`TaskError::UnknownVariant`] as a caller reads it, in `locale`.
pub(crate) fn unknown_variant_message(
    locale: crate::i18n::Locale,
    wanted: &str,
    offered: &str,
) -> String {
    crate::i18n::t_slots(
        locale,
        "Unknown variant '{variant}' (the model offers: {offered}).",
        &[("variant", wanted), ("offered", offered)],
    )
}

/// A completion summary as stored: one line of at most [`SUMMARY_CHARS`];
/// `None` when it is blank. Larger than [`MAX_SUMMARY_BYTES`] is refused.
pub(crate) fn summary_line(text: &str) -> Result<Option<String>, TaskError> {
    if text.len() > MAX_SUMMARY_BYTES {
        return Err(TaskError::InvalidSummary);
    }
    let line = one_line(text, SUMMARY_CHARS);
    Ok((!line.is_empty()).then_some(line))
}

/// A task's `progress`: one line of at most this many characters.
pub(crate) const PROGRESS_CHARS: usize = 120;
/// The most often a task's progress alone publishes it, and the most often a
/// session's timeline is read for it.
const PROGRESS_EVERY: Duration = Duration::from_secs(2);

pub(crate) struct TaskService {
    store: Arc<dyn TaskStore>,
    events: broadcast::Sender<TaskDomainEvent>,
    machine_id: String,
    /// When each linked computer last answered: a mirror's view says
    /// whether its computer answers (`peer_reachable`, §10.12).
    presence: Arc<PeerPresence>,
    /// When this gateway started ([`TaskService::holds`]).
    started_ms: u64,
    /// Pane delegations a follower is running for (see [`pane_agent::follow`]).
    following: Mutex<HashSet<TaskId>>,
    /// What each open task is doing now (see [`TaskService::note_progress`]).
    /// Kept in memory: it is live state, gone with the task's end or a restart.
    live: Mutex<Live>,
}

#[derive(Default)]
struct Live {
    lines: HashMap<TaskId, LiveLine>,
    /// The open task each agent session works for, as last looked up: `None`
    /// for a session no open task is bound to (a chat), asked again after
    /// [`UNBOUND_RECHECK`].
    sessions: HashMap<String, (Option<TaskId>, Instant)>,
    /// The name each open session delegation's agent session gave itself,
    /// as last seen (see [`TaskService::session_named`]); kept on the task
    /// when it ends.
    titles: HashMap<TaskId, String>,
    /// The open session delegations whose session the follower saw go idle
    /// since their last `tasks.complete` block (cleared by the block): the
    /// reporting turn ended ([`TaskService::holds`]). Presence, not a time,
    /// so a block and an idle in the same millisecond are still ordered.
    idled: HashSet<TaskId>,
    /// Whether a task is bound to each agent session asked about
    /// ([`TaskService::is_task_session`]); a delegation's session is marked
    /// when it is bound, before its first prompt.
    task_sessions: HashMap<String, bool>,
    /// The agent sessions running a turn now (their last status was busy):
    /// their last line may still be being written ([`super::transcript`]).
    busy: HashSet<String>,
    /// The child each open parent last delegated and has not been told the
    /// end of yet: its `progress` while it waits ([`TaskService::progress_of`]).
    waiting_on: HashMap<TaskId, WaitingOn>,
}

/// A parent's untold delegation: which, to whom, and since when.
struct WaitingOn {
    child: TaskId,
    who: String,
    since_ms: u64,
}

/// One open task's progress.
#[derive(Default)]
struct LiveLine {
    line: String,
    /// The line last published, and when.
    published: Option<String>,
    at: Option<Instant>,
    /// A trailing publish is scheduled.
    armed: bool,
}

/// How long a session found bound to no open task is not asked again.
const UNBOUND_RECHECK: Duration = Duration::from_secs(30);

pub(crate) struct DelegateRequest {
    pub goal: String,
    pub cwd: Option<String>,
    pub agent_id: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
    /// The model's variant (reasoning effort); applied only together with `model`.
    pub variant: Option<String>,
    pub machine: Option<String>,
    /// A short title for the task.
    pub label: Option<String>,
    /// The computers the user named for it (see [`Task::mentions`]).
    pub mentions: Vec<String>,
    /// See [`Task::client_ref`].
    pub client_ref: Option<String>,
    /// The terminal session a terminal agent starts in (a tmux session or
    /// Herdr, by its id in the config); `None`: the first that answers.
    pub session_id: Option<String>,
    /// Files sent with the goal, as the session prompt takes them (uploaded
    /// asset references or file paths on this computer): handed to a
    /// session agent with its first prompt.
    pub attachments: Vec<String>,
}

/// Who asked for a task, as a task view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RequestedBy {
    pub kind: PrincipalKind,
    pub id: String,
    pub name: String,
    /// Another computer's own id, as it paired here: how the phone tells
    /// which of its computers asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskView {
    pub task_id: String,
    pub state: TaskState,
    pub kind: TaskKind,
    /// The delegated goal, or the command line of a command task.
    pub goal: String,
    pub summary: Option<String>,
    pub exit_code: Option<i32>,
    pub binding: TaskBinding,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub reopened_ms: Option<u64>,
    pub reopen_count: u64,
    /// The open task this one was started from, if any.
    pub parent_task_id: Option<String>,
    pub origin: TaskOrigin,
    pub label: Option<String>,
    /// What to call the task: its label, else its agent session's own name,
    /// else its goal's first sentence (see [`pane_agent::title`]). Made when
    /// the view is, so it follows the session's name.
    pub title: String,
    /// The device that opened the task (`origin: device`); otherwise null.
    pub requested_by: Option<RequestedBy>,
    /// The direct children, counted by state when the view is made.
    pub children: ChildCounts,
    /// What an open task is doing now, one line: the agent's current tool
    /// call or the start of its latest message (a session), or the pane's
    /// last prose line (a terminal agent). Absent when nothing is known yet,
    /// and once the task has ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// A mirror's computer answered within the last two minutes; absent on
    /// every task that is not a mirror (§10.12).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_reachable: Option<bool>,
    /// When a mirror's computer last answered, if it ever did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_last_seen_ms: Option<u64>,
    /// The computers the user named for it; absent when none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mentions: Vec<String>,
    /// The requester's reference for it, when it gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_ref: Option<String>,
    /// A terminal delegation's model and effort: those its launch passed,
    /// else (Codex) its own config's, else `default` (the agent's own).
    /// Absent on every other task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// A mirror's why, for the phone to word (its summary is prose):
    /// `peer_unlinked`, `peer_lost_task` or `peer_not_confirmed` on a failed
    /// one, `peer_unreachable` on an open one. Made when the view is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// How the agent working on a task says it ended (`tasks.complete`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// Finished: the task is `done`.
    #[default]
    Done,
    /// Stopped because it needs the user (a missing file, an unclear
    /// request, a permission it cannot get): the task is `waiting`.
    Blocked,
}

/// Why a task is blocked ([`TaskService::blocked_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Blocked {
    /// When it was recorded.
    pub at_ms: u64,
    /// By the blocked marker (read once the reporting turn had ended), not
    /// by `tasks.complete` (called while it still ran).
    pub by_marker: bool,
}

/// A task as [`TaskService::wait`] leaves it, with a command's
/// output so far.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Waited {
    #[serde(flatten)]
    pub task: TaskView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_tail: Option<String>,
    /// A finished delegation's last assistant message in full; `summary` is
    /// only its first line's worth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadWhat {
    Summary,
    Timeline,
}

#[derive(Debug)]
pub(crate) enum TaskError {
    NotFound,
    /// Someone else's task. Callers outside the gateway are told
    /// [`TaskError::NotFound`]'s answer, so ids cannot be probed.
    NotYours,
    /// `machine` names neither this computer nor a linked one: what was
    /// asked for, and the linked computers' names.
    UnknownMachine {
        wanted: String,
        known: Vec<String>,
    },
    /// The linked computer of that name has not answered lately.
    MachineUnreachable(String),
    /// The linked computer of that name takes no work from other computers.
    DelegationsDisabled(String),
    /// The linked computer of that name no longer knows this one's token.
    PeerUnauthorized(String),
    /// The caller's task came from another computer: it may not hand work
    /// to one in turn (one hop only). The computer it named.
    DelegationLoop(String),
    /// The caller or an ancestor could not be established reliably.
    LineageUnresolved,
    /// The linked computer refused the task with its own error code.
    PeerRefused {
        machine: String,
        code: String,
    },
    AgentUnavailable(String),
    /// `model` names nothing in the agent's catalog for that directory.
    UnknownModel(String),
    /// `variant` is not one the resolved model lists: the variant asked
    /// for, and the ones it offers, comma-separated.
    UnknownVariant {
        wanted: String,
        offered: String,
    },
    /// No configured terminal session answers, so a command has nowhere to run.
    NoTerminal,
    /// The request named a terminal session this computer has no such id for.
    TerminalUnknown(String),
    /// Attachments sent where they cannot go along: a terminal agent, or
    /// another computer (they are references on this one).
    AttachmentsUnsupported,
    /// An attachment that is no file uploaded to this computer.
    InvalidAttachment(String),
    /// A timeline was asked of a task that has none (a command).
    NoTimeline,
    /// The device confirmed a task that is not its own paused or waiting
    /// delegation.
    NotConfirmable,
    /// `tasks.complete` (done) on a task whose sub-tasks are not all over:
    /// a plain sentence naming them.
    ChildrenOpen(String),
    /// A completion summary that is empty (where one is required) or larger
    /// than [`MAX_SUMMARY_BYTES`].
    InvalidSummary,
    Store(anyhow::Error),
    Agent(AgentError),
    Backend(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ChildrenOpen(which) => write!(f, "{which}"),
            Self::NotFound => write!(f, "task not found"),
            Self::NotYours => write!(f, "task belongs to someone else"),
            Self::UnknownMachine { wanted, .. } => write!(f, "unknown machine '{wanted}'"),
            Self::MachineUnreachable(name) => write!(f, "{name} is not reachable"),
            Self::DelegationsDisabled(name) => {
                write!(f, "{name} takes no work from other computers")
            }
            Self::PeerUnauthorized(name) => write!(f, "{name} no longer accepts this link"),
            Self::DelegationLoop(name) => {
                write!(f, "a task from another computer cannot hand work to {name}")
            }
            Self::LineageUnresolved => write!(
                f,
                "caller lineage could not be resolved; nothing was forwarded"
            ),
            Self::PeerRefused { machine, code } => write!(f, "{machine} refused the task ({code})"),
            Self::AgentUnavailable(agent) => write!(f, "agent '{agent}' is not available"),
            Self::UnknownModel(model) => write!(f, "unknown model '{model}'"),
            Self::UnknownVariant { wanted, offered } => {
                write!(
                    f,
                    "unknown variant '{wanted}' (the model offers: {offered})"
                )
            }
            Self::NoTerminal => write!(f, "{}", run_command::NO_TERMINAL),
            Self::TerminalUnknown(id) => write!(f, "no terminal session '{id}'"),
            Self::AttachmentsUnsupported => {
                write!(f, "attachments go only to a session agent on this computer")
            }
            Self::InvalidAttachment(why) => write!(f, "{why}"),
            Self::NoTimeline => write!(f, "only a delegated task has a timeline"),
            Self::InvalidSummary => write!(f, "summary must be non-empty and at most 32 KiB"),
            Self::NotConfirmable => write!(
                f,
                "only a paused or waiting task the device opened can be confirmed"
            ),
            Self::Store(error) => write!(f, "task store: {error:#}"),
            Self::Agent(error) => write!(f, "agent: {error}"),
            Self::Backend(message) => write!(f, "{message}"),
        }
    }
}

impl From<anyhow::Error> for TaskError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

impl From<AgentError> for TaskError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

/// The store as the service writes to it: every insert and transition is
/// published once it is stored. `run_command` writes through it too.
struct Publishing<'a>(&'a TaskService);

impl Publishing<'_> {
    /// Tell listeners about `task` and, since its counts moved, its parent.
    async fn publish(&self, task: &Task) {
        let service = self.0;
        if task.state.is_terminal() || paused(task.state) {
            service.forget_progress(task);
        }
        if service.events.receiver_count() == 0 {
            return;
        }
        let _ = service.events.send(TaskDomainEvent::Updated {
            task: service.present(task).await,
            seq: task.seq,
        });
        let Some(parent) = &task.parent else {
            return;
        };
        match service.store.get(parent).await {
            Ok(Some(parent)) => {
                let _ = service.events.send(TaskDomainEvent::Updated {
                    task: service.present(&parent).await,
                    seq: parent.seq,
                });
            }
            Ok(None) => {}
            Err(error) => tracing::debug!(%error, "task parent not re-published"),
        }
    }
}

impl TaskStore for Publishing<'_> {
    fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task> {
        Box::pin(async move {
            let task = self.0.store.insert(task).await?;
            self.publish(&task).await;
            Ok(task)
        })
    }

    fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
        self.0.store.get(id)
    }

    fn latest_device_cwd<'a>(
        &'a self,
        device: &'a TaskPrincipal,
    ) -> TaskFuture<'a, Option<String>> {
        self.0.store.latest_device_cwd(device)
    }

    fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>> {
        self.0.store.list(filter)
    }

    fn transition<'a>(&'a self, id: &'a TaskId, mut change: TaskChange) -> TaskFuture<'a, Task> {
        Box::pin(async move {
            // An ending task keeps its session's last name.
            if change.to.is_terminal() && change.session_title.is_none() {
                change.session_title = self.0.live_title(id);
            }
            let task = self.0.store.transition(id, change).await?;
            if task.state.is_terminal() {
                if let Ok(mut live) = self.0.live.lock() {
                    live.titles.remove(id);
                    live.idled.remove(id);
                    // Asked about again, it is read from the store again.
                    if let TaskBinding::AgentSession { asid, .. } = &task.binding {
                        live.task_sessions.remove(asid);
                    }
                }
            }
            self.publish(&task).await;
            Ok(task)
        })
    }

    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
        self.0.store.events(id)
    }

    fn child_counts<'a>(
        &'a self,
        ids: Vec<TaskId>,
    ) -> TaskFuture<'a, HashMap<TaskId, ChildCounts>> {
        self.0.store.child_counts(ids)
    }

    fn purge_ended<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize> {
        self.0.store.purge_ended(before_ms)
    }
}

fn change(to: TaskState) -> TaskChange {
    TaskChange {
        to,
        source: SOURCE.into(),
        ..Default::default()
    }
}

/// A move to `waiting` on a pending request whose text is `text`.
fn asking(text: &str) -> TaskChange {
    let line = one_line(text, SUMMARY_CHARS);
    TaskChange {
        summary: (!line.is_empty()).then_some(line),
        ..change(TaskState::Waiting)
    }
}

fn noted(to: TaskState, note: impl Into<String>) -> TaskChange {
    TaskChange {
        note: Some(note.into()),
        ..change(to)
    }
}

/// What a session event means for the task bound to it.
enum Step {
    /// Nothing changes.
    Stay,
    /// The agent picked the prompt up.
    Busy,
    /// Idle: the turn ended, if the agent went busy first (see [`turn_end`]).
    Idle,
    /// The session failed, with the agent's error if it gave one.
    Failed(Option<AgentErrorInfo>),
    /// A question form is pending, with its title ([`form_waiting`]).
    Form(String),
    Move(Box<TaskChange>),
}

/// Whether a session is running a turn.
pub(crate) fn is_running(status: AgentSessionStatus) -> bool {
    matches!(status, AgentSessionStatus::Busy | AgentSessionStatus::Retry)
}

fn step(event: &AgentDomainEvent, asid: &str) -> Step {
    let ours = |id: &AgentSessionId| id.0 == asid;
    match event {
        AgentDomainEvent::StatusChanged {
            asid: id,
            status,
            error,
            ..
        } if ours(id) => match status {
            AgentSessionStatus::Busy | AgentSessionStatus::Retry => Step::Busy,
            AgentSessionStatus::Idle => Step::Idle,
            AgentSessionStatus::Failed => Step::Failed(error.clone()),
            AgentSessionStatus::Interrupted => Step::Move(Box::new(change(TaskState::Cancelled))),
            AgentSessionStatus::Unknown => Step::Stay,
        },
        // A permission request's line is its own prompt; a form's, the
        // agent's question when it asked one ([`form_waiting`]).
        AgentDomainEvent::PermissionPending {
            asid: id, request, ..
        } if ours(id) => Step::Move(Box::new(asking(&request.prompt))),
        AgentDomainEvent::FormPending {
            asid: id, request, ..
        } if ours(id) => Step::Form(request.title.clone()),
        AgentDomainEvent::PermissionResolved { asid: id, .. }
        | AgentDomainEvent::FormResolved { asid: id, .. }
            if ours(id) =>
        {
            Step::Move(Box::new(change(TaskState::Working)))
        }
        _ => Step::Stay,
    }
}

impl TaskService {
    /// Resume the same assignment only after its observer verified a new turn.
    /// The sequence check prevents a delayed observer from reviving a cancelled
    /// task or overwriting another completion.
    pub(crate) async fn reopen(&self, task: &Task, at: u64) -> Result<Task, TaskError> {
        self.reopen_with_note(task, None, Some(at)).await
    }

    pub(crate) async fn reopen_with_note(
        &self,
        task: &Task,
        note: Option<String>,
        reopened_ms: Option<u64>,
    ) -> Result<Task, TaskError> {
        let now = self
            .store()
            .transition(
                &task.id,
                TaskChange {
                    to: TaskState::Working,
                    source: "new_user_turn".into(),
                    note,
                    reopen: true,
                    reopened_ms,
                    expected_seq: Some(task.seq),
                    ..Default::default()
                },
            )
            .await?;
        if let Ok(mut live) = self.live.lock() {
            live.idled.remove(&task.id);
            if let TaskBinding::AgentSession { asid, .. } = &task.binding {
                live.sessions.remove(asid);
            }
        }
        Ok(now)
    }

    pub(crate) async fn reopen_note(&self, id: &TaskId) -> Option<String> {
        self.store
            .events(id)
            .await
            .ok()?
            .into_iter()
            .rev()
            .find(|event| event.source == "new_user_turn")?
            .note
    }

    pub(crate) async fn reopen_session(
        &self,
        state: &AppState,
        task: &Task,
        items: &[TimelineItem],
    ) -> bool {
        if task.kind != TaskKind::Delegation
            || !matches!(task.state, TaskState::Done | TaskState::Failed)
        {
            return false;
        }
        let Some(started_ms) = items
            .iter()
            .filter(|item| item.role == TimelineRole::User && item.updated_ms > task.updated_ms)
            .map(|item| item.updated_ms)
            .min()
        else {
            return false;
        };
        let TaskBinding::AgentSession { agent_id, asid } = &task.binding else {
            return false;
        };
        let Some(manager) = state.agent_runtime.manager_for_agent(agent_id).await else {
            return false;
        };
        if manager.sessions().get_session(asid).await.is_err() {
            return false;
        }
        // Reconciliation may discover a turn after its answer already arrived.
        // Filtering from observation time would discard that turn's completion.
        self.reopen(task, started_ms).await.is_ok()
    }

    pub(crate) fn new(
        store: Arc<dyn TaskStore>,
        events: broadcast::Sender<TaskDomainEvent>,
        machine_id: String,
        presence: Arc<PeerPresence>,
    ) -> Self {
        Self {
            store,
            events,
            machine_id,
            presence,
            started_ms: now_ms(),
            following: Mutex::new(HashSet::new()),
            live: Mutex::default(),
        }
    }

    /// `line` is what open task `id` is doing now: kept for its views at
    /// once, and published as `task.updated` at most once per
    /// [`PROGRESS_EVERY`] -- a line held back is published when that time is
    /// up (one trailing publish per task). A state change publishes it anyway.
    pub(crate) async fn note_progress(&self, state: &AppState, id: &TaskId, line: &str) {
        let line = one_line(line, PROGRESS_CHARS);
        if line.is_empty() {
            return;
        }
        let next = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let entry = live.lines.entry(id.clone()).or_default();
            if entry.line == line {
                return;
            }
            entry.line = line;
            match entry.at {
                Some(at) if at.elapsed() < PROGRESS_EVERY => {
                    if entry.armed {
                        None
                    } else {
                        entry.armed = true;
                        Some(Some(at + PROGRESS_EVERY))
                    }
                }
                _ => Some(None),
            }
        };
        match next {
            // Published recently: once that time is up.
            Some(Some(when)) => {
                let (state, id) = (state.clone(), id.clone());
                tokio::spawn(async move {
                    tokio::time::sleep_until(when).await;
                    state.tasks.flush_progress(&id).await;
                });
            }
            Some(None) => self.publish_progress(id).await,
            // A trailing publish is already scheduled.
            None => {}
        }
    }

    /// The trailing publish of a held-back line.
    async fn flush_progress(&self, id: &TaskId) {
        let changed = match self.live.lock() {
            Ok(mut live) => match live.lines.get_mut(id) {
                Some(entry) => {
                    entry.armed = false;
                    entry.published.as_ref() != Some(&entry.line)
                }
                None => false,
            },
            Err(_) => false,
        };
        if changed {
            self.publish_progress(id).await;
        }
    }

    /// Publish `id` for its progress, unless it has stopped (then its line
    /// is dropped: its summary says why it stopped).
    async fn publish_progress(&self, id: &TaskId) {
        let task = match self.store.get(id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                self.forget_line(id);
                return;
            }
            Err(error) => {
                tracing::debug!(task = %id.0, %error, "task progress not published");
                return;
            }
        };
        if task.state.is_terminal() || paused(task.state) {
            self.forget_progress(&task);
            return;
        }
        if let Ok(mut live) = self.live.lock() {
            if let Some(entry) = live.lines.get_mut(id) {
                entry.published = Some(entry.line.clone());
                entry.at = Some(Instant::now());
            }
        }
        self.store().publish(&task).await;
    }

    fn progress_of(&self, task: &Task) -> Option<String> {
        if task.state.is_terminal() {
            return None;
        }
        let live = self.live.lock().ok()?;
        let line = (!paused(task.state))
            .then(|| live.lines.get(&task.id).map(|entry| entry.line.clone()))
            .flatten();
        // Its own turn is over (or quiet) while a child it delegated works.
        line.or_else(|| {
            live.waiting_on.get(&task.id).map(|waiting| {
                waiting_line(&waiting.who, now_ms().saturating_sub(waiting.since_ms))
            })
        })
    }

    /// `parent` now waits on `child`: until the child's end is told, the
    /// parent's progress reads `Waiting for <who> · <elapsed>`.
    pub(crate) async fn waits_on(&self, parent: &TaskId, child: &Task) {
        let who = match &child.binding {
            TaskBinding::Peer { peer_name, .. } => peer_name.clone(),
            TaskBinding::AgentSession { agent_id, .. } => agent_id.clone(),
            TaskBinding::Pane {
                agent_id: Some(agent_id),
                ..
            } => agent_id.clone(),
            _ => String::from("a sub-task"),
        };
        if let Ok(mut live) = self.live.lock() {
            live.waiting_on.insert(
                parent.clone(),
                WaitingOn {
                    child: child.id.clone(),
                    who,
                    since_ms: child.created_ms,
                },
            );
        }
        if let Ok(Some(parent)) = self.store.get(parent).await {
            self.store().publish(&parent).await;
        }
    }

    /// Publish every parent still waiting on a child, so its elapsed time
    /// moves on the phone.
    pub(crate) async fn republish_waiting_parents(&self) {
        let parents: Vec<TaskId> = self
            .live
            .lock()
            .map(|live| live.waiting_on.keys().cloned().collect())
            .unwrap_or_default();
        for id in parents {
            match self.store.get(&id).await {
                Ok(Some(parent)) if !parent.state.is_terminal() => {
                    self.store().publish(&parent).await
                }
                _ => {
                    if let Ok(mut live) = self.live.lock() {
                        live.waiting_on.remove(&id);
                    }
                }
            }
        }
    }

    fn forget_line(&self, id: &TaskId) {
        if let Ok(mut live) = self.live.lock() {
            live.lines.remove(id);
        }
    }

    /// Drop `task`'s progress; once it has ended, its session's binding too.
    fn forget_progress(&self, task: &Task) {
        if let Ok(mut live) = self.live.lock() {
            live.lines.remove(&task.id);
            if let (true, TaskBinding::AgentSession { asid, .. }) =
                (task.state.is_terminal(), &task.binding)
            {
                live.sessions.remove(asid);
            }
        }
    }

    /// Agent session `asid` now works for task `id` (it was just bound).
    fn bind_session(&self, asid: &str, id: &TaskId) {
        if let Ok(mut live) = self.live.lock() {
            live.sessions
                .insert(asid.to_owned(), (Some(id.clone()), Instant::now()));
            live.task_sessions.insert(asid.to_owned(), true);
        }
    }

    fn mark_task_session(&self, asid: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.task_sessions.insert(asid.to_owned(), true);
        }
    }

    /// Tell `target` how task `id` ends (`tasks::notices`).
    pub(crate) async fn notify(&self, id: &TaskId, target: &NoticeTarget) {
        if let Err(error) = self.store.set_notice(id, target).await {
            tracing::warn!(task = %id.0, %error, "the parent will not be told how this task ends");
        }
    }

    /// Where task `id`'s end is told, if anywhere.
    pub(crate) async fn notice_of(&self, id: &TaskId) -> Option<NoticeTarget> {
        self.store.notice_of(id).await.ok().flatten()
    }

    /// Untold reported states of tasks with a notice target (`tasks::notices`).
    pub(crate) async fn pending_notices(
        &self,
        since_ms: u64,
        limit: usize,
    ) -> anyhow::Result<Vec<(Task, NoticeTarget)>> {
        self.store.pending_notices(since_ms, limit).await
    }

    /// Task `id`'s `state` has reached its parent: delivered, or answered by
    /// a wait. It is not told again.
    pub(crate) async fn told(&self, id: &TaskId, state: TaskState) {
        if !REPORTED_STATES.contains(&state) {
            return;
        }
        let Ok(Some(child)) = self.store.get(id).await else {
            return;
        };
        // Moved on since: that newer state is still to be told.
        if child.state != state {
            return;
        }
        self.told_task(&child).await;
    }

    /// `child`, as it was when its line was sent (captured before the send),
    /// has been told: a move it made meanwhile is still to be told.
    pub(crate) async fn told_task(&self, child: &Task) {
        if let Err(error) = self.store.mark_told(child, now_ms()).await {
            tracing::warn!(task = %child.id.0, %error, "could not record that the parent was told");
        }
        // Its parent waits no more: the progress line goes.
        let Some(parent) = child.parent.clone() else {
            return;
        };
        let cleared = self.live.lock().is_ok_and(|mut live| {
            let ours = live
                .waiting_on
                .get(&parent)
                .is_some_and(|waiting| waiting.child == child.id);
            ours && live.waiting_on.remove(&parent).is_some()
        });
        if cleared {
            if let Ok(Some(parent)) = self.store.get(&parent).await {
                self.store().publish(&parent).await;
            }
        }
    }

    /// Delegation `parent`, when it is bound to pane `pane_id`: whether its
    /// follower saw its agent stop (idle, or it reported done), and the job
    /// its launch recorded there. Never another delegation that pane id
    /// holds now (a restarted tmux reuses ids). `None`: `parent` is no
    /// delegation bound to that pane.
    pub(crate) async fn pane_task(
        &self,
        parent: &TaskId,
        session_id: &str,
        pane_id: &str,
    ) -> Option<(TaskState, Option<pane_agent::PaneJob>)> {
        let task = self.store.get(parent).await.ok()??;
        let TaskBinding::Pane {
            session_id: bound_session,
            pane_id: bound_pane,
            shell_pid,
            agent_job,
            ..
        } = &task.binding
        else {
            return None;
        };
        if bound_session != session_id || bound_pane != pane_id {
            return None;
        }
        let job = shell_pid
            .zip(agent_job.clone())
            .map(|(shell_pid, job)| pane_agent::PaneJob { shell_pid, job });
        (task.kind == TaskKind::Delegation).then_some((task.state, job))
    }

    /// The agent sessions running a turn now, as the follower last saw them.
    pub(crate) fn busy_sessions(&self) -> Vec<String> {
        self.live
            .lock()
            .map(|live| live.busy.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether agent session `asid` is running a turn, as the follower last
    /// saw its status.
    pub(crate) fn is_busy(&self, asid: &str) -> bool {
        self.live.lock().is_ok_and(|live| live.busy.contains(asid))
    }

    /// Whether an open task is bound to pane `pane_id` of terminal session
    /// `session_id`: its agent was started for that task.
    pub(crate) async fn open_task_in_pane(&self, session_id: &str, pane_id: &str) -> bool {
        self.open_task_of_pane(session_id, pane_id).await.is_some()
    }

    /// The open task bound to a pane, if one is.
    pub(crate) async fn open_task_of_pane(&self, session_id: &str, pane_id: &str) -> Option<Task> {
        let filter = TaskFilter {
            pane: Some((session_id.to_owned(), pane_id.to_owned())),
            states: Some(OPEN_STATES.to_vec()),
            limit: 1,
            ..Default::default()
        };
        self.store.list(filter).await.ok()?.into_iter().next()
    }

    /// Whether any task, open or ended, is bound to agent session `asid`:
    /// its transcript is served without the task plumbing
    /// ([`super::transcript`]). Read from the store once per session, then
    /// remembered.
    pub(crate) async fn is_task_session(&self, asid: &str) -> bool {
        let known = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.task_sessions.get(asid).copied());
        if let Some(known) = known {
            return known;
        }
        let filter = TaskFilter {
            asid: Some(asid.to_owned()),
            limit: 1,
            ..Default::default()
        };
        let Ok(bound) = self.store.list(filter).await.map(|tasks| !tasks.is_empty()) else {
            return false;
        };
        if let Ok(mut live) = self.live.lock() {
            // Bounded: the "not a task" answers are dropped first (read
            // again when asked); a task's session stays until its task ends.
            if live.task_sessions.len() >= TASK_SESSION_CACHE {
                live.task_sessions.retain(|_, bound| *bound);
            }
            // A bind meanwhile wins over the read.
            let entry = live.task_sessions.entry(asid.to_owned()).or_insert(bound);
            return *entry;
        }
        bound
    }

    /// A session's timeline changed: the open task bound to it is doing
    /// what its newest item says.
    async fn follow_progress(&self, state: &AppState, asid: &str, items: &[TimelineItem]) {
        let Some(task) = self.task_for_session(asid).await else {
            return;
        };
        if let Some(line) = items.iter().rev().find_map(progress_line) {
            self.note_progress(state, &task, &line).await;
        }
    }

    /// The open task agent session `asid` works for, looked up once per
    /// session (a session bound to none is asked again after
    /// [`UNBOUND_RECHECK`]).
    async fn task_for_session(&self, asid: &str) -> Option<TaskId> {
        let known = self.live.lock().ok()?.sessions.get(asid).cloned();
        match known {
            Some((Some(id), _)) => Some(id),
            Some((None, at)) if at.elapsed() < UNBOUND_RECHECK => None,
            _ => {
                let found = self.open_task_for(asid).await.map(|task| task.id);
                if let Ok(mut live) = self.live.lock() {
                    live.sessions
                        .insert(asid.to_owned(), (found.clone(), Instant::now()));
                }
                found
            }
        }
    }

    /// Whether the caller is now the one follower of `id`'s pane agent.
    pub(crate) fn claim_follow(&self, id: &TaskId) -> bool {
        self.following
            .lock()
            .map(|mut ids| ids.insert(id.clone()))
            .unwrap_or(false)
    }

    pub(crate) fn release_follow(&self, id: &TaskId) {
        if let Ok(mut ids) = self.following.lock() {
            ids.remove(id);
        }
    }

    /// Refuse peer ancestry and any unresolved lineage before forwarding.
    /// This is a workflow boundary, not a sandbox for same-user shell code.
    async fn allow_forwarding(&self, by: &Requested, machine: &str) -> Result<(), TaskError> {
        if by.origin == TaskOrigin::Peer || by.by.kind == PrincipalKind::Gateway {
            return Err(TaskError::DelegationLoop(machine.to_owned()));
        }
        if by.by.kind == PrincipalKind::Assistant
            && by.forwarding != crate::tasks::domain::ForwardingContext::Resolved
        {
            return Err(TaskError::LineageUnresolved);
        }
        let mut next = by.parent.clone();
        let mut visited = std::collections::HashSet::new();
        for _ in 0..64 {
            let Some(id) = next else {
                return Ok(());
            };
            if !visited.insert(id.clone()) {
                return Err(TaskError::LineageUnresolved);
            }
            match self.store.get(&id).await {
                Ok(Some(task)) if task.origin == TaskOrigin::Peer => {
                    return Err(TaskError::DelegationLoop(machine.to_owned()));
                }
                Ok(Some(task))
                    if task.parent.is_none()
                        && task.created_by.kind == PrincipalKind::Assistant
                        && matches!(task.origin, TaskOrigin::Agent | TaskOrigin::Mcp) =>
                {
                    return Err(TaskError::LineageUnresolved);
                }
                Ok(Some(task)) => next = task.parent,
                _ => return Err(TaskError::LineageUnresolved),
            }
        }
        Err(TaskError::LineageUnresolved)
    }

    /// The task `id` as stored.
    pub(crate) async fn task(&self, id: &TaskId) -> anyhow::Result<Option<Task>> {
        self.store.get(id).await
    }

    pub(crate) async fn latest_device_cwd(
        &self,
        device: &TaskPrincipal,
    ) -> anyhow::Result<Option<String>> {
        self.store.latest_device_cwd(device).await
    }

    fn store(&self) -> Publishing<'_> {
        Publishing(self)
    }

    /// Hand `request.goal` to an agent on this machine in a new session.
    ///
    /// The task is recorded before the session exists, so a store that
    /// refuses the task leaves no session behind; once the session exists,
    /// any later failure interrupts it (best effort) and fails the task.
    pub(crate) async fn delegate(
        &self,
        state: &AppState,
        by: Requested,
        request: DelegateRequest,
    ) -> Result<TaskView, TaskError> {
        if !is_self(
            request.machine.as_deref(),
            &self.machine_id,
            &state.config.label,
            &state.peers.names(),
        ) {
            if !request.attachments.is_empty() {
                return Err(TaskError::AttachmentsUnsupported);
            }
            let wanted = request.machine.clone().unwrap_or_default();
            let Some(peer) = state.peers.resolve(&wanted).await else {
                let known = state.peers.names();
                return Err(TaskError::UnknownMachine { wanted, known });
            };
            self.allow_forwarding(&by, &peer.name).await?;
            return crate::fleet::mirror::delegate(state, by, peer, request).await;
        }
        if let Some(id) = request.session_id.as_deref() {
            if !state.config.sessions.iter().any(|session| session.id == id) {
                return Err(TaskError::TerminalUnknown(id.to_owned()));
            }
        }
        if let Some(kind) = request.agent_id.as_deref() {
            if runs_in_pane(state, kind).await {
                if !request.attachments.is_empty() {
                    return Err(TaskError::AttachmentsUnsupported);
                }
                return self.delegate_to_pane(state, by, request).await;
            }
        }
        let manager = agent_for(state, request.agent_id.as_deref()).await?;
        let agent_id = manager.agent().kind();
        // Checked before anything is made: a reference the prompt would
        // refuse is a refusal here, never a failed task and a stray session.
        if !request.attachments.is_empty() {
            let uploads = crate::uploads_dir()
                .map_err(|_| TaskError::InvalidAttachment("no upload directory".into()))?;
            crate::agents::use_cases::prompt_service::checked_attachments(
                manager.agent().attachment_mode(),
                &request.attachments,
                &uploads,
            )
            .map_err(|error| TaskError::InvalidAttachment(error.to_string()))?;
        }
        let variant = request.variant.as_deref();
        let model = match request.model.as_deref() {
            Some(id) => {
                let catalog = manager
                    .sessions()
                    .get_catalog(request.cwd.as_deref())
                    .await?;
                Some(
                    resolve_model(&catalog.models, id, variant)
                        .ok_or_else(|| TaskError::UnknownModel(id.to_owned()))?
                        .map_err(|(wanted, offered)| TaskError::UnknownVariant {
                            wanted,
                            offered,
                        })?,
                )
            }
            None => None,
        };
        let now = now_ms();
        let task = self
            .store()
            .insert(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: request.goal.clone(),
                cwd: request.cwd.clone(),
                binding: TaskBinding::None,
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: by.by,
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: by.parent,
                origin: by.origin,
                label: request.label.clone(),
                requester_name: by.name,
                session_title: None,
                mentions: request.mentions.clone(),
                client_ref: request.client_ref.clone(),
                requester_server_id: by.server_id.clone(),
                reopened_ms: None,
                reopen_count: 0,
            })
            .await?;
        let created = manager
            .sessions()
            .create_session(
                request.cwd.as_deref(),
                model.as_ref(),
                request.mode.as_deref(),
            )
            .await;
        let asid = match created {
            Ok(session) => session.asid,
            Err(error) => {
                let error = TaskError::from(error);
                self.record_failure(&task, noted(TaskState::Failed, error.to_string()))
                    .await;
                return Err(error);
            }
        };
        state
            .agent_runtime
            .record_session_route(&asid.0, agent_id)
            .await;
        let binding = TaskBinding::AgentSession {
            agent_id: agent_id.to_owned(),
            asid: asid.0.clone(),
        };
        // The task's sentence for this session only, outside its
        // conversation, where the agent takes one; removed when the task
        // ends (`driver::spawn_instruction_cleanup`).
        let mut unannounced = None;
        if crate::mcp::inject::enabled(&state.config) && manager.agent().adds_mcp_servers() {
            // Served as a task's session from here, so the notice the
            // instruction may leave at once is stripped like the rest.
            self.mark_task_session(&asid.0);
            let fleet = fleet_line(state, &task).await;
            match manager
                .agent()
                .set_session_instruction(
                    &asid.0,
                    crate::mcp::inject::SESSION_INSTRUCTION_KEY,
                    &crate::mcp::inject::task_sentence(fleet.as_deref()),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    unannounced = Some(
                        "the agent took no per-session instruction for this session; it is \
                         not told to report the task's end"
                            .to_owned(),
                    )
                }
                Err(error) => {
                    tracing::warn!(%error, "the task's session instruction was not set");
                    unannounced = Some(
                        "the task's session instruction was not set; it may end paused".to_owned(),
                    )
                }
            }
        }
        let dispatched = async {
            let dispatching = TaskChange {
                binding: Some(binding.clone()),
                note: unannounced.clone(),
                ..change(TaskState::Dispatching)
            };
            self.store().transition(&task.id, dispatching).await?;
            self.bind_session(&asid.0, &task.id);
            // The goal alone: how to say it ended is the MCP server's, which
            // the agent's service is given now if it does not have it.
            crate::mcp::inject::ensure_session_agent(
                &state.config,
                state.mcp_tokens.as_ref(),
                manager.agent().as_ref(),
                request.cwd.as_deref(),
            )
            .await;
            manager
                .prompts()
                .send_prompt(&asid, &request.goal, &request.attachments, None)
                .await?;
            let task = self
                .store()
                .transition(&task.id, change(TaskState::Working))
                .await?;
            Ok::<_, TaskError>(task)
        };
        match dispatched.await {
            Ok(task) => Ok(self
                .present(&self.settle_if_finished(state, task).await)
                .await),
            // The follower moved the task first, from the session's own events
            // (it failed, was interrupted or already finished): the task and
            // its session exist and the task says where it stands. Nothing
            // to interrupt.
            Err(error) if is_illegal_transition(&error) => {
                let now = self.store.get(&task.id).await?.ok_or(TaskError::NotFound)?;
                if !matches!(now.state, TaskState::Queued | TaskState::Dispatching) {
                    return Ok(self.present(&now).await);
                }
                Err(error)
            }
            Err(error) => {
                if let Err(interrupt) = manager.prompts().interrupt(&asid).await {
                    tracing::warn!(
                        "task {}: could not stop the session: {interrupt}",
                        task.id.0
                    );
                }
                let failed = TaskChange {
                    binding: Some(binding),
                    ..noted(TaskState::Failed, error.to_string())
                };
                self.record_failure(&task, failed).await;
                Err(error)
            }
        }
    }

    /// Hand `request.goal` to a terminal agent (`request.agent_id`, a kind
    /// the agent catalog lists): a new pane in the first reachable terminal
    /// session, the agent started in it and the goal typed as its first
    /// prompt (asking for the completion marker, see [`pane_agent`]), the
    /// way the spawn route does it. `mode`, `model` and `variant` do not
    /// apply and are ignored.
    ///
    /// Nothing is recorded when the agent is not on `PATH` or no terminal
    /// answers. The answer comes as soon as the pane exists: the task is
    /// `dispatching`, bound to the pane, and moves to `working` once the agent
    /// is up and has the goal -- or to `failed`, naming the step -- in the
    /// background, since starting an agent can take half a minute.
    async fn delegate_to_pane(
        &self,
        state: &AppState,
        by: Requested,
        request: DelegateRequest,
    ) -> Result<TaskView, TaskError> {
        let kind = request.agent_id.clone().unwrap_or_default();
        if !pane_agent::is_available(state, &kind) {
            return Err(TaskError::AgentUnavailable(kind));
        }
        let session = match request.session_id.as_deref() {
            Some(id) => {
                let session = state
                    .config
                    .sessions
                    .iter()
                    .find(|session| session.id == id)
                    .cloned()
                    .ok_or_else(|| TaskError::TerminalUnknown(id.to_owned()))?;
                if !matches!(
                    crate::terminal_backend(&session).probe_reachable().await,
                    Ok(true)
                ) {
                    return Err(TaskError::NoTerminal);
                }
                session
            }
            None => {
                run_command::reachable_session(state)
                    .await
                    .ok_or(TaskError::NoTerminal)?
                    .0
            }
        };
        let (model_args, model, variant) = pane_agent::launch_model(
            state,
            &kind,
            request.model.as_deref(),
            request.variant.as_deref(),
        )
        .await?;
        let now = now_ms();
        let task = self
            .store()
            .insert(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: request.goal.clone(),
                cwd: request.cwd.clone(),
                binding: TaskBinding::None,
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: by.by,
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: by.parent,
                origin: by.origin,
                label: request.label.clone(),
                requester_name: by.name,
                session_title: None,
                mentions: request.mentions.clone(),
                client_ref: request.client_ref.clone(),
                requester_server_id: by.server_id.clone(),
                reopened_ms: None,
                reopen_count: 0,
            })
            .await?;
        let cwd = request.cwd.as_deref();
        let placed =
            match crate::agents::session_routes::place_agent(state, &session, &kind, cwd, None)
                .await
            {
                Ok(placed) if !placed.place.pane_id.is_empty() => placed,
                outcome => {
                    let message = match outcome {
                        Err(error) => error.to_string(),
                        Ok(_) => "no pane could be made".to_owned(),
                    };
                    let error = TaskError::Backend(format!("{kind}: {message}"));
                    self.record_failure(&task, noted(TaskState::Failed, error.to_string()))
                        .await;
                    return Err(error);
                }
            };
        let pane_id = placed.place.pane_id.clone();
        let backend = crate::terminal_backend(&session);
        let shell_pid = backend
            .list_panes()
            .await
            .ok()
            .and_then(|panes| panes.into_iter().find(|p| p.id.as_str() == pane_id))
            .and_then(|pane| pane.shell_pid);
        // Named for the task rather than for the directory or the command.
        let title = pane_agent::title(request.label.as_deref(), None, &request.goal);
        if let Some(tab_id) = &placed.place.tab_id {
            if let Err(error) = backend.rename_tab(&TabId::new(tab_id), &title).await {
                tracing::debug!(%error, "terminal task: the tab keeps its name");
            }
        }
        if let Err(error) = backend.rename_pane(&PaneId::new(&pane_id), &title).await {
            tracing::debug!(%error, "terminal task: the pane keeps its title");
        }
        let binding = TaskBinding::Pane {
            session_id: session.id.clone(),
            pane_id: pane_id.clone(),
            tab_id: placed.place.tab_id.clone(),
            shell_pid,
            agent_id: Some(kind.clone()),
            agent_job: None,
            model,
            variant,
        };
        let dispatching = TaskChange {
            binding: Some(binding),
            ..change(TaskState::Dispatching)
        };
        let task = match self.store().transition(&task.id, dispatching).await {
            Ok(task) => task,
            Err(error) => {
                tracing::warn!(task = %task.id.0, pane = %pane_id, "terminal task: pane left unbound");
                return Err(error.into());
            }
        };
        let view = self.present(&task).await;
        let state = state.clone();
        let start = TerminalStart {
            cwd: request.cwd.clone(),
            goal: request.goal.clone(),
            model_args,
        };
        tokio::spawn(async move {
            let start = tokio::spawn(start_terminal_agent(
                state.clone(),
                session,
                kind,
                placed,
                start,
                task.clone(),
            ));
            // A start that panicked must not leave the task dispatching.
            if let Err(error) = start.await {
                let why = if error.is_panic() {
                    "the agent start failed: it panicked".to_owned()
                } else {
                    "the agent start failed: it was cancelled".to_owned()
                };
                state
                    .tasks
                    .record_failure(&task, noted(TaskState::Failed, why))
                    .await;
            }
        });
        Ok(view)
    }
}

/// Whether a delegation to agent `kind` starts it in a terminal pane: a
/// terminal agent from the catalog that no session runtime serves. A
/// session agent (OpenCode, DeepSeek, T3) never is: unattached, it is
/// unavailable. Explicit Codex task requests retain their existing terminal
/// route, independent of the private Codex agent-session adapter's readiness.
pub(crate) async fn runs_in_pane(state: &AppState, kind: &str) -> bool {
    if kind == "codex" {
        return pane_agent::is_terminal_agent(state, kind);
    }
    !AGENT_PREFERENCE.contains(&kind)
        && state.agent_runtime.manager_for_agent(kind).await.is_none()
        && pane_agent::is_terminal_agent(state, kind)
}

/// What [`start_terminal_agent`] starts the agent with: where, the goal, and
/// the launch arguments for the model and effort the request named.
pub(crate) struct TerminalStart {
    pub cwd: Option<String>,
    pub goal: String,
    pub model_args: Vec<String>,
}

/// The background half of [`TaskService::delegate_to_pane`]: start the agent
/// in its placed pane with the goal alone (on its command line where it
/// takes one, otherwise typed) unless the task ended meanwhile -- a cancel, or a
/// follower that gave up on it -- then move the task to working and follow
/// it -- or fail it naming the step.
/// The computers a task's agent is told about: for a task the owner opened
/// here, the linked computers and the ones the owner named; a task another
/// computer sent over is told nothing (one hop, and no further names).
pub(crate) async fn fleet_line(state: &AppState, task: &Task) -> Option<String> {
    if task.origin != TaskOrigin::Device {
        return None;
    }
    crate::fleet::manifest::prompt_line(state, &task.mentions).await
}

pub(crate) async fn start_terminal_agent(
    state: AppState,
    session: crate::SessionConfig,
    kind: String,
    placed: crate::agents::session_routes::PlacedAgent,
    start: TerminalStart,
    task: Task,
) {
    let TerminalStart {
        cwd,
        goal,
        model_args,
    } = start;
    use crate::agents::session_routes::{start_agent_in_place, type_agent_prompt, TaskLaunch};
    let tasks = &state.tasks;
    // A cancel (or a follower that gave up) while the pane was made or the
    // agent started: nothing more is started or typed.
    let ended = || async {
        match tasks.store.get(&task.id).await {
            Ok(Some(now)) => now.state.is_terminal(),
            Ok(None) => true,
            Err(_) => false,
        }
    };
    if ended().await {
        tracing::debug!(task = %task.id.0, "terminal task ended before its agent started");
        return;
    }
    let pane_id = placed.place.pane_id.clone();
    let mut goal = goal;
    let fleet = fleet_line(&state, &task).await;
    // What ran before the launch command was typed is never its job.
    let mut before = pane_agent::before_launch().await;
    let (mut launched, mut to_type) = start_agent_in_place(
        &state,
        &session,
        &kind,
        cwd.as_deref(),
        placed.clone(),
        Some(&goal),
        Some(TaskLaunch {
            task_id: &task.id.0,
            out_of_band: true,
            model_args: &model_args,
            fleet: fleet.as_deref(),
        }),
    )
    .await;
    // Retry an explicit startup failure only when no goal submission was
    // attempted. An argv goal can already have taken effect before an exit
    // or error; a shell foreground alone never proves a rejected goal.
    let out_of_band = kind == "claude"
        && crate::mcp::inject::enabled(&state.config)
        && !crate::mcp::inject::terminal_task_args(&state.config, &kind, None)
            .await
            .is_empty();
    if out_of_band
        && may_retry_typed(&launched)
        && pane_agent::exited_to_shell(&session, &pane_id, &kind).await
        && !ended().await
    {
        tracing::info!(task = %task.id.0, "terminal task: started again with the sentence typed");
        goal = format!("{goal}\n\n{}", crate::mcp::inject::TASK_SENTENCE);
        before = pane_agent::before_launch().await;
        (launched, to_type) = start_agent_in_place(
            &state,
            &session,
            &kind,
            cwd.as_deref(),
            placed,
            Some(&goal),
            Some(TaskLaunch {
                task_id: &task.id.0,
                out_of_band: false,
                fleet: None,
                model_args: &model_args,
            }),
        )
        .await;
    }
    let typed = to_type.is_some();
    if let Some(started) = to_type {
        if ended().await {
            tracing::debug!(task = %task.id.0, "terminal task ended while its agent started: goal not typed");
            return;
        }
        type_agent_prompt(&session, &started, Some(&goal), &mut launched).await;
    }
    let why = match failed_step(&launched.steps.value()) {
        Some(why) => Some(why),
        // Typed is not sent: Claude Code can keep a pasted goal in its box.
        None if typed && !pane_agent::goal_accepted(&session, &pane_id, &goal).await => {
            Some(pane_agent::NOT_ACCEPTED.to_owned())
        }
        None => None,
    };
    if let Some(why) = why {
        tasks
            .record_failure(&task, noted(TaskState::Failed, why))
            .await;
        return;
    }
    let mut to = change(TaskState::Working);
    // Preserve the shared-start refusal in the existing task event history.
    // A usable terminal launch must not imply it acquired native RPC control.
    to.note = shared_session_fallback_note(&launched.steps.value());
    let mut binding = task.binding.clone();
    if let TaskBinding::Pane {
        shell_pid,
        agent_job,
        ..
    } = &mut binding
    {
        // The job the launch started: the only one a told result is ever
        // typed to (`tasks::notices`). Without one, nothing ever is.
        *agent_job = match (*shell_pid, &before) {
            (Some(shell_pid), Some(before)) => pane_agent::launched_job(shell_pid, before).await,
            _ => None,
        };
        if agent_job.is_none() {
            tracing::warn!(
                task = %task.id.0,
                "terminal task: no launched job recorded; its pane will not be typed to"
            );
        }
    }
    if binding != task.binding {
        to.binding = Some(binding);
    }
    // A follower or a cancel may have moved it meanwhile; follow whatever is
    // still open.
    if let Err(error) = tasks.store().transition(&task.id, to).await {
        tracing::debug!(task = %task.id.0, %error, "terminal task: not moved to working");
    }
    pane_agent::follow(&state, &task);
}

impl TaskService {
    /// Move `task` as `to` says, through whatever states lie between
    /// (Queued passes Dispatching, Waiting passes Working), reading the
    /// task's current state first. The steps carry `to`'s source.
    pub(crate) async fn move_to(&self, task: &TaskId, to: TaskChange) -> Result<Task, TaskError> {
        // A verified new generation is one atomic transition, including a
        // peer that completed and reopened between two Working snapshots.
        if to.reopen {
            return Ok(self.store().transition(task, to).await?);
        }
        let now = self.store.get(task).await?.ok_or(TaskError::NotFound)?;
        for step in steps_between(now.state, to.to) {
            let step = TaskChange {
                to: step,
                source: to.source.clone(),
                ..Default::default()
            };
            self.store().transition(task, step).await?;
        }
        Ok(self.store().transition(task, to).await?)
    }

    /// A just-dispatched task whose session already finished its turn (its
    /// idle event came before the task was Working, when the follower could
    /// not settle it): settled now (see [`turn_end`]), not at the driver's
    /// next pass.
    async fn settle_if_finished(&self, state: &AppState, task: Task) -> Task {
        let Some(done) = self.finished_change(state, &task).await else {
            return task;
        };
        match self.settle(&task, done).await {
            Ok(done) => done,
            Err(error) => {
                tracing::debug!(task = %task.id.0, %error, "a finished task was not settled");
                self.store
                    .get(&task.id)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(task)
            }
        }
    }

    /// Store a new task as it is given, and publish it.
    pub(crate) async fn record(&self, task: &Task) -> Result<Task, TaskError> {
        Ok(self.store().insert(task).await?)
    }

    /// One `task.updated` with `task`'s current view, though nothing about
    /// it moved: what its view derives (a mirror's `peer_reachable`) did.
    pub(crate) async fn republish(&self, task: &Task) {
        let task = match self.store.get(&task.id).await {
            Ok(Some(now)) => now,
            _ => task.clone(),
        };
        let _ = self.events.send(TaskDomainEvent::Updated {
            task: self.present(&task).await,
            seq: task.seq,
        });
    }

    /// Whether `task` mirrors a task on a computer that has not answered lately.
    fn peer_quiet(&self, task: &Task) -> bool {
        match &task.binding {
            TaskBinding::Peer { peer_id, .. } => !self.presence.of(peer_id, now_ms()).0,
            _ => false,
        }
    }

    /// [`Self::move_to`], logging instead of failing: the caller is already
    /// reporting an error. The failure's note is its summary, unless it has one.
    pub(crate) async fn record_failure(&self, task: &Task, mut failed: TaskChange) {
        if failed.summary.is_none() {
            failed.summary = failed
                .note
                .as_deref()
                .map(|note| one_line(note, SUMMARY_CHARS));
        }
        if let Err(store) = self.move_to(&task.id, failed).await {
            tracing::warn!("task {}: could not record the failure: {store}", task.id.0);
        }
    }

    /// Wait up to `timeout` (1..=45 s) for the task to move: to done, failed
    /// or cancelled, to waiting on the user, or (a delegation) to
    /// `idle_unconfirmed`, its agent stopped without saying it is finished. A
    /// command's answer carries what it printed so far; a session
    /// delegation's, done or paused, the agent's last message (`answer`).
    pub(crate) async fn wait(
        &self,
        state: &AppState,
        by: &TaskPrincipal,
        id: &TaskId,
        timeout: Duration,
    ) -> Result<Waited, TaskError> {
        let deadline = Instant::now() + timeout.clamp(MIN_WAIT, MAX_WAIT);
        let task = self.owned(by, id).await?;
        let view = match &task.binding {
            _ if task.state.is_terminal()
                && !(task.kind == TaskKind::Command
                    && matches!(task.binding, TaskBinding::Peer { .. })) =>
            {
                self.present(&task).await
            }
            TaskBinding::Pane { .. } if task.kind == TaskKind::Delegation => {
                self.wait_published(&task.id, deadline).await?
            }
            // A mirror whose computer is quiet is answered at once, saying
            // so (`peer_reachable: false`); the agent decides (§10.14).
            TaskBinding::Peer { .. } if self.peer_quiet(&task) => self.present(&task).await,
            // A command on another computer: its tail and exit code come
            // from there once it has ended.
            TaskBinding::Peer { .. } if task.kind == TaskKind::Command => {
                let view = self.wait_published(&task.id, deadline).await?;
                let mut stdout_tail = None;
                let mut view = view;
                if view.state.is_terminal() {
                    if let Some(now) = self.task(&task.id).await.ok().flatten() {
                        if let Some((code, tail)) =
                            crate::fleet::mirror::command_result(state, &now).await
                        {
                            stdout_tail = Some(tail);
                            view.exit_code = view.exit_code.or(code);
                        }
                    }
                }
                return Ok(Waited {
                    task: view,
                    stdout_tail,
                    answer: None,
                });
            }
            TaskBinding::Peer { .. } => self.wait_published(&task.id, deadline).await?,
            TaskBinding::Pane { .. } => {
                let handle = self.poll_command(state, &task, deadline).await?;
                return Ok(Waited {
                    task: self.present(&handle.task).await,
                    stdout_tail: Some(handle.stdout_tail),
                    answer: None,
                });
            }
            TaskBinding::AgentSession { agent_id, asid } => {
                self.wait_session(state, task.clone(), agent_id, asid, deadline)
                    .await?
            }
            TaskBinding::None => self.present(&task).await,
        };
        let answer = match &view.binding {
            TaskBinding::AgentSession { asid, .. }
                if matches!(view.state, TaskState::Done | TaskState::IdleUnconfirmed) =>
            {
                full_answer(state, asid, task.created_ms).await
            }
            _ => None,
        };
        Ok(Waited {
            task: view,
            stdout_tail: None,
            answer,
        })
    }

    async fn wait_session(
        &self,
        state: &AppState,
        mut task: Task,
        agent_id: &str,
        asid: &str,
        deadline: Instant,
    ) -> Result<TaskView, TaskError> {
        // Subscribe before reading the session, so nothing falls between.
        let mut events = state.agent_runtime.subscribe_events();
        let manager = session_manager(state, agent_id, asid).await?;
        let since = task.reopened_ms.unwrap_or(task.created_ms);
        let mut pending: VecDeque<_> = snapshot(&manager, asid, since, task.state).await?.into();
        let mut seen_busy = false;
        loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Ok(event)) => event,
                    // Skipped events may have held the busy spell: ask again.
                    Ok(Err(RecvError::Lagged(_))) => {
                        pending.extend(snapshot(&manager, asid, since, task.state).await?);
                        continue;
                    }
                    Ok(Err(RecvError::Closed)) | Err(_) => return Ok(self.present(&task).await),
                },
            };
            let to = match step(&event, asid) {
                Step::Stay => continue,
                Step::Busy => {
                    seen_busy = true;
                    continue;
                }
                Step::Idle if !seen_busy => continue,
                Step::Idle => turn_end(&turn_timeline(&manager, asid, &task).await),
                Step::Failed(error) => failed_change(&manager, &task, error.as_ref()).await,
                Step::Form(title) => form_waiting(&manager, &task, &title).await,
                Step::Move(to) => *to,
            };
            if to.to == task.state {
                // The follower may have moved it since it was read (a paused
                // task working again).
                let now = self.store.get(&task.id).await?.ok_or(TaskError::NotFound)?;
                if now.state == task.state {
                    // Still waiting on the user, or still paused: say so now,
                    // there is nothing to wait for until someone acts.
                    if paused(task.state) {
                        return Ok(self.present(&task).await);
                    }
                    continue;
                }
                task = now;
            }
            task = match self.settle(&task, to).await {
                Ok(task) => task,
                // Moved meanwhile (another waiter, a cancel): that is the answer.
                Err(error) if is_illegal_transition(&error) => {
                    let now = self.store.get(&task.id).await?.ok_or(TaskError::NotFound)?;
                    return Ok(self.present(&now).await);
                }
                Err(error) => return Err(error),
            };
            if task.state.is_terminal() || paused(task.state) {
                return Ok(self.present(&task).await);
            }
        }
    }

    /// Wait for the task to be recorded as finished, waiting on the user or
    /// paused (by whoever moves it), or -- a mirror -- for its computer to go
    /// quiet, or for `deadline`; the task as it then stands.
    async fn wait_published(&self, id: &TaskId, deadline: Instant) -> Result<TaskView, TaskError> {
        let settled =
            |task: &Task| task.state.is_terminal() || paused(task.state) || self.peer_quiet(task);
        // Subscribe before reading, so nothing falls between.
        let mut events = self.events.subscribe();
        loop {
            let task = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
            if settled(&task) {
                return Ok(self.present(&task).await);
            }
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Ok(Ok(TaskDomainEvent::Updated { task, .. })) if task.task_id == id.0 => break,
                    Ok(Ok(_)) => {}
                    Ok(Err(RecvError::Lagged(_))) => break,
                    Ok(Err(RecvError::Closed)) | Err(_) => return Ok(self.present(&task).await),
                }
            }
        }
    }

    /// Move `task` as `to` says, through the states between (a task waiting
    /// on the user that finishes passes Working; so does an unconfirmed idle
    /// one that fails) -- unless it is blocked and stays ([`Self::holds`]):
    /// then it is answered as it stands.
    pub(crate) async fn settle(&self, task: &Task, to: TaskChange) -> Result<Task, TaskError> {
        if self.holds(task, to.to).await {
            return Ok(self
                .store
                .get(&task.id)
                .await?
                .unwrap_or_else(|| task.clone()));
        }
        self.move_to(&task.id, to).await
    }

    /// `Summary`: the task as it stands. `Timeline`: the agent session's
    /// items after `cursor`, the last [`TIMELINE_PAGE`] of them, and the
    /// cursor to read on from.
    pub(crate) async fn read(
        &self,
        state: &AppState,
        by: &TaskPrincipal,
        id: &TaskId,
        what: ReadWhat,
        cursor: Option<u64>,
    ) -> Result<Value, TaskError> {
        let task = self.owned(by, id).await?;
        self.read_task(state, task, what, cursor).await
    }

    /// [`read`](Self::read) of the task the caller is working on, which it
    /// did not create (a device did): the caller showed it is its own
    /// (`mcp::lineage::own_task`).
    pub(crate) async fn read_own(
        &self,
        state: &AppState,
        id: &TaskId,
        what: ReadWhat,
        cursor: Option<u64>,
    ) -> Result<Value, TaskError> {
        let task = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
        self.read_task(state, task, what, cursor).await
    }

    async fn read_task(
        &self,
        state: &AppState,
        task: Task,
        what: ReadWhat,
        cursor: Option<u64>,
    ) -> Result<Value, TaskError> {
        if what == ReadWhat::Summary {
            return Ok(json!(self.present(&task).await));
        }
        let TaskBinding::AgentSession { agent_id, asid } = &task.binding else {
            return Err(TaskError::NoTimeline);
        };
        let manager = session_manager(state, agent_id, asid).await?;
        let items = manager.agent().get_timeline(asid, TIMELINE_READ).await?;
        let after = cursor.unwrap_or(0);
        let fresh: Vec<&TimelineItem> = items.iter().filter(|item| item.seq > after).collect();
        let page = &fresh[fresh.len().saturating_sub(TIMELINE_PAGE)..];
        let next = page.iter().map(|item| item.seq).max().unwrap_or(after);
        let mut items: Vec<Value> = page.iter().map(|item| compact_item(item)).collect();
        // Newest first until the byte bound; at least the newest item.
        let mut used = 0;
        let kept = items
            .iter()
            .rev()
            .take_while(|item| {
                used += item.to_string().len() + 1;
                used <= TIMELINE_PAGE_BYTES
            })
            .count()
            .max(items.len().min(1));
        let left_out = items.len() - kept;
        items.drain(..left_out);
        let mut page = json!({ "task_id": task.id.0, "items": items, "cursor": next });
        if left_out > 0 {
            page["truncated"] = Value::Bool(true);
        }
        Ok(page)
    }

    /// Interrupt the agent (or Ctrl-C the command), best effort, and mark the
    /// task Cancelled: an agent or pane that cannot be reached must not keep
    /// a task open that its owner gave up on.
    pub(crate) async fn cancel(
        &self,
        state: &AppState,
        by: &TaskPrincipal,
        id: &TaskId,
    ) -> Result<TaskView, TaskError> {
        let task = self.owned(by, id).await?;
        if task.state.is_terminal() {
            return Ok(self.present(&task).await);
        }
        match &task.binding {
            TaskBinding::Pane { .. } if task.kind == TaskKind::Delegation => {
                pane_agent::interrupt(state, &task).await;
            }
            TaskBinding::Pane { .. } => run_command::interrupt(state, &task).await,
            TaskBinding::AgentSession { agent_id, asid } => {
                let interrupted = match session_manager(state, agent_id, asid).await {
                    Ok(manager) => manager
                        .prompts()
                        .interrupt(&AgentSessionId(asid.clone()))
                        .await
                        .map_err(TaskError::from),
                    Err(error) => Err(error),
                };
                if let Err(error) = interrupted {
                    tracing::warn!(task = %task.id.0, %error, "could not interrupt the agent session");
                }
            }
            TaskBinding::Peer { .. } => crate::fleet::mirror::forward_cancel(state, &task).await,
            TaskBinding::None => {}
        }
        let task = match self
            .store()
            .transition(&task.id, change(TaskState::Cancelled))
            .await
        {
            Ok(task) => task,
            // It ended meanwhile (the interrupt finished it): that is the answer.
            Err(error) => match self.store.get(&task.id).await? {
                Some(ended) if ended.state.is_terminal() => ended,
                _ => return Err(error.into()),
            },
        };
        Ok(self.present(&task).await)
    }

    /// Tasks `by` may see, newest first: all of them for a device or no one
    /// in particular, otherwise only the ones `by` created.
    pub(crate) async fn list_for(
        &self,
        by: Option<&TaskPrincipal>,
        states: Option<Vec<TaskState>>,
        limit: usize,
    ) -> anyhow::Result<Vec<TaskView>> {
        let created_by = by.filter(|by| by.kind != PrincipalKind::Device).cloned();
        let tasks = self
            .store
            .list(TaskFilter {
                created_by,
                states,
                limit,
                ..Default::default()
            })
            .await?;
        self.present_all(&tasks).await
    }

    /// `root` and every task started from it or from its descendants, newest
    /// first; empty when `root` does not exist. For devices, who see every task.
    pub(crate) async fn tree(
        &self,
        root: &TaskId,
        states: Option<Vec<TaskState>>,
        limit: usize,
    ) -> anyhow::Result<Vec<TaskView>> {
        let tasks = self
            .store
            .list(TaskFilter {
                root: Some(root.clone()),
                states,
                limit,
                ..Default::default()
            })
            .await?;
        self.present_all(&tasks).await
    }

    /// The task `id` and its state history, if `by` may see it.
    pub(crate) async fn history(
        &self,
        by: &TaskPrincipal,
        id: &TaskId,
    ) -> Result<(TaskView, Vec<TaskEvent>), TaskError> {
        let task = self.owned(by, id).await?;
        let events = self.store.events(id).await?;
        Ok((self.present(&task).await, events))
    }

    /// The task `id`, if `by` may see it.
    pub(crate) async fn owned(&self, by: &TaskPrincipal, id: &TaskId) -> Result<Task, TaskError> {
        let task = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
        if by.kind == PrincipalKind::Device || task.created_by == *by {
            Ok(task)
        } else {
            Err(TaskError::NotYours)
        }
    }

    /// Start a command task (`exec.run`) on this machine and wait up to
    /// `request.timeout` for it to finish.
    pub(crate) async fn run(
        &self,
        state: &AppState,
        by: Requested,
        request: RunRequest,
    ) -> Result<RunHandle, TaskError> {
        let handle = run_command::start(state, &self.store(), by, request)
            .await
            .map_err(command_error)?;
        run_command::follow_peer_command(state, &handle.task);
        Ok(handle)
    }

    /// Run `command` on the linked computer `machine` (`exec.run` with
    /// `machine`): a command task in that computer's ledger, mirrored here
    /// under the caller's task, waited for up to `timeout` in all. One hop
    /// only, as for a delegation.
    pub(crate) async fn run_on(
        &self,
        state: &AppState,
        by: Requested,
        machine: &str,
        command: String,
        cwd: Option<String>,
        timeout: Duration,
    ) -> Result<crate::fleet::mirror::RemoteRun, TaskError> {
        let Some(peer) = state.peers.resolve(machine).await else {
            let known = state.peers.names();
            return Err(TaskError::UnknownMachine {
                wanted: machine.to_owned(),
                known,
            });
        };
        self.allow_forwarding(&by, &peer.name).await?;
        let owner = by.by.clone();
        let started = Instant::now();
        let mut run =
            crate::fleet::mirror::run_command(state, by, peer, command, cwd, timeout).await?;
        let left = timeout.saturating_sub(started.elapsed());
        if !run.task.state.is_terminal() && left >= MIN_WAIT {
            let id = TaskId(run.task.task_id.clone());
            let waited = self.wait(state, &owner, &id, left).await?;
            run.task = waited.task;
            run.stdout_tail = waited.stdout_tail.or(run.stdout_tail);
        }
        if run.task.state.is_terminal() && run.exit_code.is_none() {
            run.exit_code = run.task.exit_code;
        }
        Ok(run)
    }

    /// Open tasks for the driver, as stored.
    pub(crate) async fn open_tasks(&self, filter: TaskFilter) -> anyhow::Result<Vec<Task>> {
        self.store.list(filter).await
    }

    /// Forget the tasks that ended more than [`TASK_RETENTION`] ago, with
    /// their events. How many went.
    pub(crate) async fn purge_ended(&self) -> anyhow::Result<usize> {
        let before = now_ms().saturating_sub(TASK_RETENTION.as_millis() as u64);
        self.store.purge_ended(before).await
    }

    /// Read a command task's pane once: it completes if it has exited.
    pub(crate) async fn poll_once(
        &self,
        state: &AppState,
        task: &Task,
    ) -> Result<RunHandle, TaskError> {
        self.poll_command(state, task, Instant::now()).await
    }

    #[cfg(test)]
    pub(crate) async fn insert_for_test(&self, task: &Task) -> TaskId {
        self.store().insert(task).await.unwrap().id
    }

    #[cfg(test)]
    pub(crate) async fn events_for_test(&self, id: &TaskId) -> Vec<TaskEvent> {
        self.store.events(id).await.unwrap()
    }

    #[cfg(test)]
    pub(crate) async fn get_for_test(&self, id: &TaskId) -> Task {
        self.store.get(id).await.unwrap().unwrap()
    }

    /// Read a command task's pane until it exits or `deadline` passes.
    async fn poll_command(
        &self,
        state: &AppState,
        task: &Task,
        deadline: Instant,
    ) -> Result<RunHandle, TaskError> {
        run_command::poll(state, &self.store(), task, deadline)
            .await
            .map_err(command_error)
    }

    /// Task `id` as clients see it, whoever asks: for a session reading its
    /// own task (`tasks.read` without a task_id).
    pub(crate) async fn own_view(&self, id: &TaskId) -> Result<TaskView, TaskError> {
        let task = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
        Ok(self.present(&task).await)
    }

    /// `task` as clients see it, with its children counted.
    pub(crate) async fn present(&self, task: &Task) -> TaskView {
        let counts = match self.store.child_counts(vec![task.id.clone()]).await {
            Ok(mut counts) => counts.remove(&task.id).unwrap_or_default(),
            Err(error) => {
                tracing::warn!(task = %task.id.0, %error, "task children not counted");
                ChildCounts::default()
            }
        };
        self.presented(task, counts)
    }

    /// [`Self::view`] with what only the service knows: the live progress
    /// line and title, and a mirror's computer's presence.
    fn presented(&self, task: &Task, children: ChildCounts) -> TaskView {
        let view = Self::view(task, children);
        let (peer_reachable, peer_last_seen_ms) = match &task.binding {
            TaskBinding::Peer { peer_id, .. } => {
                let (reachable, seen) = self.presence.of(peer_id, now_ms());
                (Some(reachable), seen)
            }
            _ => (None, None),
        };
        TaskView {
            progress: self.progress_of(task),
            title: self.title_of(task).unwrap_or(view.title),
            peer_reachable,
            peer_last_seen_ms,
            reason: crate::fleet::mirror::reason(task, peer_reachable),
            ..view
        }
    }

    /// [`Self::present`] for a page of tasks, counted in one query.
    async fn present_all(&self, tasks: &[Task]) -> anyhow::Result<Vec<TaskView>> {
        let ids = tasks.iter().map(|task| task.id.clone()).collect();
        let counts = self.store.child_counts(ids).await?;
        Ok(tasks
            .iter()
            .map(|task| self.presented(task, counts.get(&task.id).copied().unwrap_or_default()))
            .collect())
    }

    pub(crate) fn view(task: &Task, children: ChildCounts) -> TaskView {
        let requested_by = match (&task.origin, &task.requester_name) {
            (TaskOrigin::Device | TaskOrigin::Peer, Some(name)) => Some(RequestedBy {
                kind: task.created_by.kind,
                id: task.created_by.id.clone(),
                name: name.clone(),
                server_id: task.requester_server_id.clone(),
            }),
            _ => None,
        };
        let (model, variant) = match &task.binding {
            TaskBinding::Pane {
                agent_id: Some(_),
                model,
                variant,
                ..
            } => (
                Some(model.clone().unwrap_or_else(|| "default".to_owned())),
                variant.clone(),
            ),
            _ => (None, None),
        };
        TaskView {
            task_id: task.id.0.clone(),
            state: task.state,
            kind: task.kind,
            goal: task.goal.clone(),
            summary: task.summary.clone(),
            exit_code: task.exit_code,
            binding: task.binding.clone(),
            created_ms: task.created_ms,
            updated_ms: task.updated_ms,
            reopened_ms: task.reopened_ms,
            reopen_count: task.reopen_count,
            parent_task_id: task.parent.as_ref().map(|id| id.0.clone()),
            origin: task.origin,
            label: task.label.clone(),
            title: pane_agent::title(task.label.as_deref(), None, &task.goal),
            requested_by,
            children,
            progress: None,
            peer_reachable: None,
            peer_last_seen_ms: None,
            mentions: task.mentions.clone(),
            client_ref: task.client_ref.clone(),
            model,
            variant,
            reason: None,
        }
    }

    /// `task`'s title from its session's name -- for a mirror, the title the
    /// task over there shows: the live name for an open task, the one kept
    /// when it ended for the rest; never its asid, nor its goal restated or
    /// cut short.
    fn title_of(&self, task: &Task) -> Option<String> {
        let asid = match &task.binding {
            TaskBinding::AgentSession { asid, .. } => asid.as_str(),
            TaskBinding::Peer { .. } => "",
            _ => return None,
        };
        if task.label.is_some() {
            return None;
        }
        let name = match task.state.is_terminal() {
            true => task.session_title.clone()?,
            false => self.live_title(&task.id)?,
        };
        let name = one_line(&name, usize::MAX);
        let short = name.trim_end_matches(['…', '.']).trim_end();
        let goal = one_line(&task.goal, usize::MAX);
        let says_more = !short.is_empty() && name != asid && !goal.starts_with(short);
        says_more.then(|| pane_agent::title(None, Some(&name), &task.goal))
    }

    fn live_title(&self, id: &TaskId) -> Option<String> {
        self.live.lock().ok()?.titles.get(id).cloned()
    }

    /// Mirror `id`'s task over there shows `title`: the mirror shows it too,
    /// so one task reads the same on both computers' cards.
    pub(crate) async fn mirror_titled(&self, id: &TaskId, title: &str) {
        let title = one_line(title, SUMMARY_CHARS);
        if title.is_empty() {
            return;
        }
        let changed = self.live.lock().is_ok_and(|mut live| {
            live.titles.insert(id.clone(), title.clone()).as_deref() != Some(title.as_str())
        });
        if changed {
            if let Ok(Some(task)) = self.store.get(id).await {
                self.store().publish(&task).await;
            }
        }
    }

    /// Agent session `asid` is named `title` now: remembered, when an open
    /// task works in it, for that task's title, and the task re-published
    /// when that changes it. A session the provider has not named carries
    /// its adapter's placeholder (OpenCode's id, DeepSeek's
    /// "DeepSeek Agent Session", T3's "New thread"; see
    /// `untitled_placeholder`), which never names a task, as an empty title
    /// does not.
    pub(crate) async fn session_named(&self, asid: &str, title: &str) {
        let title = title.trim();
        if title.is_empty() || untitled_placeholder(asid, title) {
            return;
        }
        let Some(id) = self.task_for_session(asid).await else {
            return;
        };
        let changed = match self.live.lock() {
            Ok(mut live) => {
                live.titles.insert(id.clone(), title.to_owned()).as_deref() != Some(title)
            }
            Err(_) => return,
        };
        if !changed {
            return;
        }
        if let Ok(Some(task)) = self.store.get(&id).await {
            self.store().publish(&task).await;
        }
    }

    /// Settle the open task bound to the session an agent event is about, as
    /// it happens, so the ledger moves without anyone waiting: a finished
    /// turn is done on its completion marker and `idle_unconfirmed` without
    /// one (see [`turn_end`]), a paused task whose session runs again
    /// working, a failed session failed with its error line, an interrupted
    /// one cancelled, a pending request waiting. Whether a task moved.
    pub(crate) async fn follow(&self, state: &AppState, event: &AgentDomainEvent) -> bool {
        if let AgentDomainEvent::StatusChanged { asid, status, .. } = event {
            if let Ok(mut live) = self.live.lock() {
                if matches!(status, AgentSessionStatus::Busy | AgentSessionStatus::Retry) {
                    live.busy.insert(asid.0.clone());
                } else {
                    live.busy.remove(&asid.0);
                }
            }
        }
        match event {
            AgentDomainEvent::TimelineUpsert { asid, items, .. } => {
                let latest = self
                    .store
                    .list(TaskFilter {
                        asid: Some(asid.0.clone()),
                        limit: 1,
                        ..Default::default()
                    })
                    .await
                    .unwrap_or_default();
                let mut reopened = false;
                if let Some(task) = latest.first() {
                    reopened = self.reopen_session(state, task, items).await;
                }
                self.follow_progress(state, &asid.0, items).await;
                return reopened;
            }
            AgentDomainEvent::SessionUpdated { asid, info, .. } => {
                self.session_named(&asid.0, &info.title).await;
                return false;
            }
            _ => {}
        }
        let asid = match event {
            AgentDomainEvent::StatusChanged {
                asid,
                status:
                    AgentSessionStatus::Idle
                    | AgentSessionStatus::Busy
                    | AgentSessionStatus::Retry
                    | AgentSessionStatus::Failed
                    | AgentSessionStatus::Interrupted,
                ..
            }
            | AgentDomainEvent::PermissionPending { asid, .. }
            | AgentDomainEvent::FormPending { asid, .. }
            | AgentDomainEvent::PermissionResolved { asid, .. }
            | AgentDomainEvent::FormResolved { asid, .. } => asid,
            _ => return false,
        };
        let Some(task) = self.open_task_for(&asid.0).await else {
            return false;
        };
        let to = match step(event, &asid.0) {
            Step::Idle => {
                self.idle_seen(&task);
                match self.finished_change(state, &task).await {
                    Some(done) => done,
                    None => return false,
                }
            }
            // A new turn after a pause: the user (or the agent) went on. A
            // blocked task's own turn may still be running ([`Self::settle`]).
            Step::Busy if task.state == TaskState::IdleUnconfirmed => change(TaskState::Working),
            Step::Busy
                if task.state == TaskState::Waiting
                    && self.blocked_at(&task.id).await.is_some() =>
            {
                change(TaskState::Working)
            }
            Step::Form(title) if task.state != TaskState::Waiting => {
                match state.agent_runtime.manager_for_session(&asid.0).await {
                    Some(manager) => form_waiting(&manager, &task, &title).await,
                    None => asking(&title),
                }
            }
            Step::Failed(error) => match state.agent_runtime.manager_for_session(&asid.0).await {
                Some(manager) => failed_change(&manager, &task, error.as_ref()).await,
                None => failed(
                    session_failure(None, error.as_ref(), &asid.0, 0).await,
                    error,
                ),
            },
            Step::Move(to) if to.to != task.state => *to,
            _ => return false,
        };
        match self.settle(&task, sourced(to, FOLLOWER)).await {
            Ok(now) => now.seq != task.seq,
            Err(error) => {
                // Moved meanwhile by a waiter or the driver: nothing to do.
                tracing::debug!(task = %task.id.0, %error, "task follower: move refused");
                false
            }
        }
    }

    /// The open task bound to agent session `asid`, if any.
    async fn open_task_for(&self, asid: &str) -> Option<Task> {
        let open = self
            .store
            .list(TaskFilter {
                asid: Some(asid.to_owned()),
                states: Some(OPEN_STATES.to_vec()),
                limit: 1,
                ..Default::default()
            })
            .await;
        match open {
            Ok(tasks) => tasks.into_iter().next(),
            Err(error) => {
                tracing::debug!(%error, "task lookup by session failed");
                None
            }
        }
    }

    /// What the ended turn makes of `task` ([`turn_end`]), if its session
    /// went idle after the task was created (it worked on the prompt in
    /// between) and that moves the task.
    async fn finished_change(&self, state: &AppState, task: &Task) -> Option<TaskChange> {
        let TaskBinding::AgentSession { agent_id, asid } = &task.binding else {
            return None;
        };
        let manager = state.agent_runtime.manager_for_agent(agent_id).await?;
        let session = manager.sessions().get_session(asid).await.ok()?;
        if !finished_since(&session, task.reopened_ms.unwrap_or(task.created_ms)) {
            return None;
        }
        let to = turn_end(&turn_timeline(&manager, asid, task).await);
        self.moves(task, &to).await.then_some(to)
    }

    /// Whether `to` moves `task`: a change of state, or a task blocked by
    /// its agent blocked again for a new reason (which replaces the old).
    pub(crate) async fn moves(&self, task: &Task, to: &TaskChange) -> bool {
        if to.to != task.state {
            return true;
        }
        to.to == TaskState::Waiting
            && to.summary.is_some()
            && to.summary != task.summary
            && self.blocked_at(&task.id).await.is_some()
    }

    /// The agent working on `id` says how it ended (`tasks.complete`; the
    /// caller has checked the agent owns the task): done, or blocked
    /// (`waiting`), with `summary`. A task that has already ended, or is
    /// already where it would move, is answered as it stands.
    pub(crate) async fn complete(
        &self,
        id: &TaskId,
        summary: &str,
        outcome: Outcome,
    ) -> Result<TaskView, TaskError> {
        let (to, source) = match outcome {
            Outcome::Done => (TaskState::Done, "tasks_complete"),
            Outcome::Blocked => (TaskState::Waiting, COMPLETE_BLOCKED),
        };
        let to = TaskChange {
            summary: Some(summary_line(summary)?.ok_or(TaskError::InvalidSummary)?),
            source: source.into(),
            ..change(to)
        };
        if outcome == Outcome::Blocked {
            // The reporting turn has not been seen to end yet.
            if let Ok(mut live) = self.live.lock() {
                live.idled.remove(id);
            }
        } else if let Some(open) = self.open_children(id).await {
            // Done means the whole of it: a child still out (working,
            // paused, waiting) is finished or cancelled first.
            return Err(TaskError::ChildrenOpen(open));
        }
        self.finish(id, to).await
    }

    /// A sentence naming task `id`'s sub-tasks that have not ended, if any:
    /// `mini is still on 'Checking Xcode version'; wait for it or cancel it.`
    async fn open_children(&self, id: &TaskId) -> Option<String> {
        let filter = TaskFilter {
            root: Some(id.clone()),
            states: Some(OPEN_STATES.to_vec()),
            limit: 20,
            ..Default::default()
        };
        let open: Vec<String> = self
            .store
            .list(filter)
            .await
            .ok()?
            .into_iter()
            .filter(|task| task.id != *id)
            .map(|child| {
                let who = match &child.binding {
                    TaskBinding::Peer { peer_name, .. } => peer_name.clone(),
                    _ => String::from("A sub-task"),
                };
                let title = self.title_of(&child).unwrap_or_else(|| {
                    pane_agent::title(
                        child.label.as_deref(),
                        child.session_title.as_deref(),
                        &child.goal,
                    )
                });
                format!("{who} is still on '{title}'")
            })
            .collect();
        (!open.is_empty()).then(|| {
            let it = if open.len() == 1 { "it" } else { "them" };
            format!("{}; wait for {it} or cancel {it}.", open.join("; "))
        })
    }

    /// Whether the task `id` is `waiting` because its agent said it is
    /// blocked (`tasks.complete` with outcome `blocked`, or the blocked
    /// marker), and how: its last event says so.
    pub(crate) async fn blocked_at(&self, id: &TaskId) -> Option<Blocked> {
        let events = self.store.events(id).await.ok()?;
        let last = events.last()?;
        let by_marker = last.source == pane_agent::MARKER_BLOCKED;
        let blocked =
            last.to == TaskState::Waiting && (by_marker || last.source == COMPLETE_BLOCKED);
        blocked.then_some(Blocked {
            at_ms: last.at_ms,
            by_marker,
        })
    }

    /// The reasons of the blocked markers task `id` was moved to `waiting`
    /// on (each such event's note).
    pub(crate) async fn marker_reasons(&self, id: &TaskId) -> Vec<String> {
        let events = self.store.events(id).await.unwrap_or_default();
        events
            .into_iter()
            .filter(|event| event.source == pane_agent::MARKER_BLOCKED)
            .filter_map(|event| event.note)
            .collect()
    }

    /// Whether a blocked task stays `waiting` instead of moving `to`: a
    /// turn that ends without the done marker does not pause it, and only a
    /// turn of its session after the one that reported the block (the user
    /// answered) is work again -- for a marker, any turn that runs, as the
    /// reporting one had ended when it was read; for `tasks.complete`, one
    /// after the follower saw the session go idle since the call, or any
    /// once this gateway started after it. A pane task's follower keeps the
    /// same rule itself ([`pane_agent::judge`]).
    async fn holds(&self, task: &Task, to: TaskState) -> bool {
        if !matches!(to, TaskState::Working | TaskState::IdleUnconfirmed) {
            return false;
        }
        let Some(blocked) = self.blocked_at(&task.id).await else {
            return false;
        };
        if !matches!(task.binding, TaskBinding::AgentSession { .. }) {
            return false;
        }
        // `working` is only ever proposed on a sign of a running turn (a busy
        // event, a running session); a session read need not agree -- an
        // OpenCode v2 read always says idle -- so only the order of events
        // decides which turn it is. One after a marker block is a new turn:
        // the marker was read on its turn's idle. One after a `tasks.complete`
        // block is, once its session was seen idle since; a gateway started
        // after the block never saw the reporting turn run, so takes the next
        // turn it sees as the answer.
        if to == TaskState::IdleUnconfirmed {
            return true;
        }
        let ended = blocked.by_marker
            || self.started_ms > blocked.at_ms
            || self
                .live
                .lock()
                .ok()
                .is_some_and(|live| live.idled.contains(&task.id));
        !ended
    }

    /// The follower saw `task`'s session go idle now.
    fn idle_seen(&self, task: &Task) {
        if let Ok(mut live) = self.live.lock() {
            live.idled.insert(task.id.clone());
        }
    }

    /// What `default_cwd` is for `device`: the folder its newest task ran
    /// in while that is still a directory, else the home directory -- named
    /// from `~` (`~`, `~/<path>`), so no user name is shown. `None` when the
    /// task store cannot be read.
    pub(crate) async fn default_cwd(&self, device: &TaskPrincipal) -> Option<String> {
        match self.store.latest_device_cwd(device).await {
            Ok(Some(dir)) if Path::new(&dir).is_dir() => {
                Some(home_relative(&dir, dirs::home_dir().as_deref()))
            }
            Ok(_) => Some("~".to_owned()),
            Err(error) => {
                tracing::debug!(%error, "default folder not read");
                None
            }
        }
    }

    /// The device confirms that a delegation it opened, paused
    /// (`idle_unconfirmed`) or waiting on the user, is finished: done, with
    /// `summary` when one is given and the hint it had otherwise. Confirming
    /// a done task again answers it as it stands.
    pub(crate) async fn confirm(
        &self,
        id: &TaskId,
        summary: Option<&str>,
    ) -> Result<TaskView, TaskError> {
        if let Some(summary) = summary {
            summary_line(summary)?;
        }
        let task = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
        // A mirror is settled here in any open state (its computer may be
        // quiet, §10.11); the task over there is left as it is.
        let mirror = matches!(task.binding, TaskBinding::Peer { .. });
        // Opened by a device, or by another of the owner's computers: the
        // phone owns both, and confirms either.
        let opened_by_owner = matches!(task.origin, TaskOrigin::Device | TaskOrigin::Peer);
        if task.state == TaskState::Done && (opened_by_owner || mirror) {
            return Ok(self.present(&task).await);
        }
        let confirmable = task.kind == TaskKind::Delegation
            && ((opened_by_owner && paused(task.state)) || (mirror && !task.state.is_terminal()));
        if !confirmable {
            return Err(TaskError::NotConfirmable);
        }
        // A blank summary keeps the hint.
        let summary = summary.map(summary_line).transpose()?.flatten();
        let done = TaskChange {
            summary,
            source: "device_confirm".into(),
            ..change(TaskState::Done)
        };
        self.finish(id, done).await
    }

    /// `to` (done, or waiting when blocked) through the states between; a
    /// task that ended meanwhile, or already stands there, is answered as
    /// it stands.
    async fn finish(&self, id: &TaskId, to: TaskChange) -> Result<TaskView, TaskError> {
        let target = to.to;
        match self.move_to(id, to).await {
            Ok(task) => Ok(self.present(&task).await),
            Err(error) if is_illegal_transition(&error) => {
                let now = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
                if now.state.is_terminal() || now.state == target {
                    return Ok(self.present(&now).await);
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

/// What a timeline item says an agent is doing, as a progress line: a tool
/// call (see [`tool_line`]; a failed call with its error), or the start of
/// an assistant message. Nothing for the rest, or for a call that says
/// nothing more than its tool's name (the line before stands).
fn progress_line(item: &TimelineItem) -> Option<String> {
    match &item.part {
        AgentPart::Tool(call) => {
            let what = tool_line(call)?;
            Some(match &call.error {
                Some(error) => format!("{what} failed: {}: {}", error.name, error.message),
                None => what,
            })
        }
        // The marker line is the end, said to the gateway, not progress.
        AgentPart::Text { text } if item.role == TimelineRole::Assistant => {
            super::transcript::without_any_marker(text)
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        }
        _ => None,
    }
}

/// A tool call in words: its title when that says more than its argument,
/// else `<tool>` and the argument it acts on -- a file (a patch's too), else
/// a command's first line, a pattern, a query or a url -- as inline code.
/// A title that only restates that argument (OpenCode's adapter derives
/// titles from them) is the argument. Never a bare name: a call whose
/// arguments name nothing readable (or have not streamed in yet) has no
/// line, and the line before stands.
fn tool_line(call: &ToolCall) -> Option<String> {
    let arg = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| call.input.get(*key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    // A patch names its files inside its text (`*** Update File: <path>`).
    let patched = || {
        arg(&["patchText", "patch"])?.lines().find_map(|line| {
            ["*** Update File:", "*** Add File:", "*** Delete File:"]
                .iter()
                .find_map(|head| line.strip_prefix(head))
                .map(str::trim)
        })
    };
    let whole = arg(&["filePath", "file_path", "path"])
        .or_else(patched)
        .or_else(|| arg(&["command", "pattern", "query", "url"]));
    let title = call
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty() && *title != call.name);
    let Some(whole) = whole else {
        return title.map(str::to_owned);
    };
    let target = whole.lines().next().unwrap_or(whole).trim();
    let restates = |title: &str| target.ends_with(title) || whole.ends_with(title);
    let line = match title {
        Some(title) if !restates(title) => title.to_owned(),
        // The whole line fits the progress line, so its fence is never cut.
        _ => {
            let room = PROGRESS_CHARS.saturating_sub(call.name.chars().count() + 1);
            format!("{} {}", call.name, code(target, room))
        }
    };
    Some(line)
}

/// `text` as inline code of at most `room` characters, `text` cut with `…`
/// to fit: the app reads progress as markdown, and a path like
/// `src/__tests__/a.ts` must not turn bold. The fence is one backtick
/// longer than any run inside.
fn code(text: &str, room: usize) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    let text = one_line(text, room.saturating_sub(2 * fence.len() + 2).max(1));
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// How many sessions' "is it a task's?" answers are remembered before the
/// negative ones are dropped.
const TASK_SESSION_CACHE: usize = 4096;

/// Whether `title` is what an adapter shows for a session that has not been
/// named (its id, T3's "New thread", DeepSeek's placeholder): never a
/// task's title.
fn untitled_placeholder(asid: &str, title: &str) -> bool {
    title == asid
        || title == crate::agents::adapters::t3::driver::NEW_THREAD_TITLE
        || title == crate::agents::adapters::deepseek::mapper::UNTITLED
}

/// A parent's progress while a child works: `Waiting for mini · 3 min`.
fn waiting_line(who: &str, elapsed_ms: u64) -> String {
    let minutes = elapsed_ms / 60_000;
    let elapsed = match minutes {
        0 => String::from("under a minute"),
        1..=59 => format!("{minutes} min"),
        _ => format!("{} h {} min", minutes / 60, minutes % 60),
    };
    format!("Waiting for {who} · {elapsed}")
}

/// Stopped short of an end: waiting on the user, or paused without saying
/// it is finished.
fn paused(state: TaskState) -> bool {
    matches!(state, TaskState::Waiting | TaskState::IdleUnconfirmed)
}

/// What an ended turn makes of a session delegation. A stop is not an end
/// -- the agent may be asking something, have stopped early, or go on after
/// a tool round -- so only the completion marker ([`pane_agent::DONE_MARKER`]
/// at the start of a line of its last message, the rule a terminal agent's
/// screen is read by) is `done`, with the marker's summary, and the blocked
/// marker is `waiting` with its reason. Without one the task is
/// `idle_unconfirmed`, its summary the last message's line as a hint.
pub(crate) fn turn_end(items: &[TimelineItem]) -> TaskChange {
    let last = last_assistant_text(items);
    match last.as_deref().and_then(pane_agent::marker) {
        Some(pane_agent::Marker::Done(summary)) => TaskChange {
            summary: Some(summary),
            ..change(TaskState::Done)
        },
        Some(pane_agent::Marker::Blocked { reason, .. }) => TaskChange {
            summary: Some(reason.clone()),
            note: Some(reason),
            source: pane_agent::MARKER_BLOCKED.into(),
            ..change(TaskState::Waiting)
        },
        // The hint is what the reader sees: never a marker's text.
        None => TaskChange {
            summary: last
                .map(|text| one_line(&super::transcript::without_any_marker(&text), SUMMARY_CHARS)),
            ..change(TaskState::IdleUnconfirmed)
        },
    }
}

/// A session task's move to `waiting` on a question form titled `title`:
/// its `summary` is the agent's own question -- the last line of its last
/// message, when that came after the turn's last tool call -- else the
/// form's title. (A permission request's line is its prompt, [`asking`].)
async fn form_waiting(manager: &AgentManager, task: &Task, title: &str) -> TaskChange {
    let TaskBinding::AgentSession { asid, .. } = &task.binding else {
        return asking(title);
    };
    let items = turn_timeline(manager, asid, task).await;
    let last_tool = items
        .iter()
        .rposition(|item| matches!(item.part, AgentPart::Tool(_)));
    let last_text = items.iter().rposition(|item| {
        item.role == TimelineRole::Assistant
            && matches!(&item.part, AgentPart::Text { text } if !text.trim().is_empty())
    });
    let asked = match (last_text, last_tool) {
        (Some(text), tool) if tool.is_none_or(|tool| text > tool) => last_assistant_text(&items)
            .map(|text| super::transcript::without_any_marker(&text))
            .and_then(|text| {
                text.lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .map(str::to_owned)
            }),
        _ => None,
    };
    asking(asked.as_deref().unwrap_or(title))
}

/// `dir` named from the home directory: `~` itself, `~/<rest>` below it,
/// else as it is. Both are canonicalized first, so a home reached through a
/// symlink still reads `~`.
fn home_relative(dir: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return dir.to_owned();
    };
    let real = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    match real(Path::new(dir)).strip_prefix(real(home)) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => dir.to_owned(),
    }
}

/// `task`'s session failed (`error`, if the agent gave one).
async fn failed_change(
    manager: &AgentManager,
    task: &Task,
    error: Option<&AgentErrorInfo>,
) -> TaskChange {
    let asid = match &task.binding {
        TaskBinding::AgentSession { asid, .. } => asid.as_str(),
        _ => "",
    };
    let why = session_failure(Some(manager), error, asid, task.created_ms).await;
    failed(why, error.cloned())
}

fn failed(why: String, error: Option<AgentErrorInfo>) -> TaskChange {
    TaskChange {
        summary: Some(why),
        note: error.map(|e| format!("{}: {}", e.name, e.message)),
        ..change(TaskState::Failed)
    }
}

/// The states a task is still open in.
pub(crate) const OPEN_STATES: [TaskState; 5] = [
    TaskState::Queued,
    TaskState::Dispatching,
    TaskState::Working,
    TaskState::Waiting,
    TaskState::IdleUnconfirmed,
];
const FOLLOWER: &str = "task_follower";
/// The source of a move to `waiting` by `tasks.complete` with outcome
/// `blocked`.
pub(crate) const COMPLETE_BLOCKED: &str = "tasks_complete_blocked";

/// `to` recorded as from `source`, unless it is a blocked marker's move,
/// which keeps saying so ([`TaskService::blocked_at`] reads it).
pub(crate) fn sourced(to: TaskChange, source: &str) -> TaskChange {
    if to.source == pane_agent::MARKER_BLOCKED {
        return to;
    }
    TaskChange {
        source: source.into(),
        ..to
    }
}
/// A failed session's summary when the agent gave no error.
pub(crate) const SESSION_FAILED: &str = "the agent session failed";

/// A failed session's summary, never just "failed": the agent's own error
/// (`<name>: <message>`), else the last thing the session's timeline says
/// went wrong (a failed tool call, a status line) or said.
pub(crate) async fn session_failure(
    manager: Option<&AgentManager>,
    error: Option<&AgentErrorInfo>,
    asid: &str,
    since_ms: u64,
) -> String {
    if let Some(error) = error {
        let agent = manager.map_or("the agent", |manager| agent_name(manager.agent().kind()));
        return provider_failure(agent, error).unwrap_or_else(|| {
            one_line(&format!("{}: {}", error.name, error.message), SUMMARY_CHARS)
        });
    }
    let Some(manager) = manager else {
        return SESSION_FAILED.to_owned();
    };
    let items = summary_timeline(manager, asid, since_ms).await;
    let detail = items.iter().rev().find_map(|item| match &item.part {
        AgentPart::Tool(call) if call.error.is_some() => progress_line(item),
        AgentPart::Status { text } if !text.trim().is_empty() => Some(text.clone()),
        AgentPart::Text { .. } => progress_line(item),
        _ => None,
    });
    match detail {
        Some(detail) => one_line(&format!("{SESSION_FAILED}: {detail}"), SUMMARY_CHARS),
        None => SESSION_FAILED.to_owned(),
    }
}

/// An agent kind as a person names it.
fn agent_name(kind: &str) -> &str {
    match kind {
        "opencode" => "OpenCode",
        "deepseek" => "DeepSeek",
        "t3" => "T3 Code",
        "claude" => "Claude Code",
        "codex" => "Codex",
        other => other,
    }
}

/// A model provider's refusal (`provider.auth …`, `ProviderAuthError`,
/// `provider.*`) in words for a person, never the raw error:
/// `OpenCode could not reach its model provider (HTTP 403)`, or `OpenCode's
/// model provider refused the request (429)`. `None`: not a provider error.
fn provider_failure(agent: &str, error: &AgentErrorInfo) -> Option<String> {
    let name = error.name.to_ascii_lowercase();
    if !name.starts_with("provider") && name != "apierror" {
        return None;
    }
    // An HTTP error status, never a port (`…:443`) that happens to have three digits.
    let status = error.status.or_else(|| {
        let message = error.message.as_bytes();
        (0..message.len().saturating_sub(2)).find_map(|at| {
            let digits = &message[at..at + 3];
            let whole = digits.iter().all(u8::is_ascii_digit)
                && (at == 0 || !message[at - 1].is_ascii_digit() && message[at - 1] != b':')
                && message
                    .get(at + 3)
                    .is_none_or(|next| !next.is_ascii_digit());
            let code: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
            (whole && (400..600).contains(&code)).then_some(code)
        })
    });
    let mut capital = agent.chars();
    let agent = match capital.next() {
        Some(first) => first.to_uppercase().chain(capital).collect::<String>(),
        None => String::new(),
    };
    Some(if name.contains("auth") {
        let status = status
            .map(|status| format!(" (HTTP {status})"))
            .unwrap_or_default();
        format!("{agent} could not reach its model provider{status}")
    } else {
        let status = status
            .map(|status| format!(" ({status})"))
            .unwrap_or_default();
        format!("{agent}'s model provider refused the request{status}")
    })
}

/// The timeline a finished delegation's summary is read from.
///
/// The agent's own read is asked first (OpenCode persists its answer before
/// it goes idle). It can lag the stream, though: DeepSeek reports the turn's
/// end from its live stream while `session/page` has yet to commit the final
/// assistant message, and then holds no answer for this turn at all. The
/// gateway's mirror of the session has the streamed rows by then, so when the
/// agent's read lacks an assistant answer written since `since_ms` (the
/// task's start), the mirror is read instead.
pub(crate) async fn turn_timeline(
    manager: &AgentManager,
    asid: &str,
    task: &Task,
) -> Vec<TimelineItem> {
    let since = task.reopened_ms.unwrap_or(task.created_ms);
    summary_timeline(manager, asid, since)
        .await
        .into_iter()
        .filter(|item| task.reopened_ms.is_none() || item.updated_ms >= since)
        .collect()
}

pub(crate) async fn summary_timeline(
    manager: &AgentManager,
    asid: &str,
    since_ms: u64,
) -> Vec<TimelineItem> {
    let read = match manager.agent().get_timeline(asid, SUMMARY_ITEMS).await {
        Ok(items) => items,
        Err(error) => {
            tracing::warn!(asid, %error, "task summary: timeline read failed");
            Vec::new()
        }
    };
    let streamed = manager
        .mirror()
        .get_snapshot(&AgentSessionId(asid.to_owned()))
        .await
        .map(|snapshot| snapshot.timeline)
        .unwrap_or_default();
    // The newer answer wins; the agent's read wins a tie, or when it has no times.
    match (
        newest_answer(&read, since_ms),
        newest_answer(&streamed, since_ms),
    ) {
        (Some(agent), Some(mirror)) if agent != 0 && mirror > agent => streamed,
        (None, Some(_)) => streamed,
        _ => read,
    }
}

/// The time of the newest assistant text row in `items` written at or after
/// `since_ms`, if any. A row without a time (0) is taken to be current: a
/// delegation's session is created for it, so no earlier turn is in it.
fn newest_answer(items: &[TimelineItem], since_ms: u64) -> Option<u64> {
    items
        .iter()
        .filter(|item| {
            item.role == TimelineRole::Assistant
                && matches!(&item.part, AgentPart::Text { text } if !text.trim().is_empty())
                && (item.updated_ms == 0 || item.updated_ms >= since_ms)
        })
        .map(|item| item.updated_ms)
        .max()
}

/// The last assistant message of `asid`, in full; `None` when it cannot be read.
pub(crate) async fn full_answer(state: &AppState, asid: &str, since_ms: u64) -> Option<String> {
    let manager = state.agent_runtime.manager_for_session(asid).await?;
    // Never the child's marker: a parent quoting it would complete itself.
    last_assistant_text(&summary_timeline(&manager, asid, since_ms).await)
        .map(|text| super::transcript::without_any_marker(&text))
}

/// Only an explicit exit before readiness, with no attempted goal delivery,
/// permits the typed-sentence fallback. Missing or ambiguous evidence fails closed.
fn may_retry_typed(launch: &crate::agents::session_routes::AgentLaunch) -> bool {
    use crate::agents::session_routes::PromptDelivery;
    if launch.prompt_delivery != PromptDelivery::NotAttempted
        || launch.payload.get("agent_started") != Some(&json!(false))
        || launch.payload.get("prompt_submitted") != Some(&json!(false))
    {
        return false;
    }
    let steps = launch.steps.value();
    let Some(steps) = steps.as_array() else {
        return false;
    };
    let mut failed_before_prompt = false;
    for step in steps {
        match (
            step.get("step").and_then(Value::as_str),
            step.get("status").and_then(Value::as_str),
        ) {
            (Some("agent"), Some("failed"))
                if !failed_before_prompt
                    && step.pointer("/error/code").and_then(Value::as_str)
                        == Some("agent_start_failed") =>
            {
                failed_before_prompt = true;
            }
            (Some("agent" | "prompt"), _) | (_, Some("failed")) | (_, None) => return false,
            (Some(_), Some("ok" | "skipped")) => {}
            _ => return false,
        }
    }
    failed_before_prompt
}

/// `"<step>: <message>"` for the first failed step of a launch.
fn failed_step(steps: &Value) -> Option<String> {
    let step = steps
        .as_array()?
        .iter()
        .find(|step| step.get("status").and_then(Value::as_str) == Some("failed"))?;
    Some(format!(
        "{}: {}",
        step.get("step").and_then(Value::as_str).unwrap_or("launch"),
        step.pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("failed")
    ))
}

fn shared_session_fallback_note(steps: &Value) -> Option<String> {
    let step = steps
        .as_array()?
        .iter()
        .find(|step| step["step"] == "shared_session" && step["status"] == "skipped")?;
    let reason = one_line(step["reason"].as_str()?, 512);
    Some(format!(
        "Started in terminal mode: {reason}. Continue in the linked terminal."
    ))
}

fn command_error(error: anyhow::Error) -> TaskError {
    if error.to_string() == run_command::NO_TERMINAL {
        TaskError::NoTerminal
    } else {
        TaskError::Backend(format!("{error:#}"))
    }
}

/// The states a task passes on its way from `from` to `to`, `to` excluded:
/// the shortest legal path, or none when `to` is directly reachable (or not
/// reachable at all -- the store then refuses the move).
fn steps_between(from: TaskState, to: TaskState) -> Vec<TaskState> {
    use TaskState::*;
    const ALL: [TaskState; 8] = [
        Queued,
        Dispatching,
        Working,
        Waiting,
        IdleUnconfirmed,
        Done,
        Failed,
        Cancelled,
    ];
    let mut paths: VecDeque<Vec<TaskState>> = VecDeque::from([vec![from]]);
    while let Some(path) = paths.pop_front() {
        let last = *path.last().expect("a path is never empty");
        if last.can_move_to(to) {
            return path[1..].to_vec();
        }
        for next in ALL {
            if last.can_move_to(next) && !path.contains(&next) && !next.is_terminal() {
                let mut longer = path.clone();
                longer.push(next);
                paths.push_back(longer);
            }
        }
    }
    Vec::new()
}

fn is_illegal_transition(error: &TaskError) -> bool {
    matches!(
        error,
        TaskError::Store(e)
            if matches!(e.downcast_ref(), Some(TaskStoreError::IllegalTransition { .. }))
    )
}

/// A session read as the events that would have led to it. Idle since
/// before `since_ms` (the prompt) says nothing; idle after it means the
/// agent was busy in between, which is how a task that finished before
/// anyone waited is still seen to finish.
fn status_events(session: AgentSessionInfo, since_ms: u64) -> Vec<AgentDomainEvent> {
    let status = |status| AgentDomainEvent::StatusChanged {
        asid: session.asid.clone(),
        status,
        error: session.error.clone(),
        seq: 0,
    };
    if finished_since(&session, since_ms) {
        return vec![
            status(AgentSessionStatus::Busy),
            status(AgentSessionStatus::Idle),
        ];
    }
    vec![status(session.status)]
}

/// Whether `session` went idle after `since_ms`: it was busy in between, so
/// a prompt sent at `since_ms` has been answered.
pub(crate) fn finished_since(session: &AgentSessionInfo, since_ms: u64) -> bool {
    session.status == AgentSessionStatus::Idle
        && session.time_idle.is_some_and(|idle| idle > since_ms)
}

/// The session as it stands, read as events: its status (see
/// [`status_events`]), then a pending permission or form request -- one
/// raised while nobody waited would otherwise go unseen. A task waiting on
/// the user whose session runs with nothing pending was answered meanwhile.
async fn snapshot(
    manager: &AgentManager,
    asid: &str,
    since_ms: u64,
    task_state: TaskState,
) -> Result<Vec<AgentDomainEvent>, TaskError> {
    let session = manager.sessions().get_session(asid).await?;
    let running = is_running(session.status);
    let id = session.asid.clone();
    let mut events = status_events(session, since_ms);
    let agent = manager.agent();
    let unsupported = |what: &str, error: AgentError| {
        tracing::debug!("task wait: no pending {what} for {asid}: {error}");
    };
    let permission = agent
        .get_pending_permissions(asid)
        .await
        .unwrap_or_else(|e| {
            unsupported("permissions", e);
            Vec::new()
        })
        .into_iter()
        .next();
    let form = agent
        .get_pending_forms(asid)
        .await
        .unwrap_or_else(|e| {
            unsupported("forms", e);
            Vec::new()
        })
        .into_iter()
        .next();
    if let Some(request) = permission {
        events.push(AgentDomainEvent::PermissionPending {
            asid: id,
            request,
            seq: 0,
        });
    } else if let Some(request) = form {
        events.push(AgentDomainEvent::FormPending {
            asid: id,
            request,
            seq: 0,
        });
    } else if task_state == TaskState::Waiting && running {
        events.push(AgentDomainEvent::PermissionResolved {
            asid: id,
            request_id: String::new(),
            seq: 0,
        });
    }
    Ok(events)
}

/// The manager `agent_id` names, or the first attached agent in preference
/// order.
async fn agent_for(
    state: &AppState,
    agent_id: Option<&str>,
) -> Result<Arc<AgentManager>, TaskError> {
    let runtime = &state.agent_runtime;
    match agent_id {
        Some(id) => runtime
            .manager_for_agent(id)
            .await
            .ok_or_else(|| TaskError::AgentUnavailable(id.to_owned())),
        None => {
            for id in AGENT_PREFERENCE {
                if let Some(manager) = runtime.manager_for_agent(id).await {
                    return Ok(manager);
                }
            }
            runtime
                .manager()
                .await
                .ok_or_else(|| TaskError::AgentUnavailable("any".into()))
        }
    }
}

async fn session_manager(
    state: &AppState,
    agent_id: &str,
    asid: &str,
) -> Result<Arc<AgentManager>, TaskError> {
    state
        .agent_runtime
        .manager_for_session(asid)
        .await
        .ok_or_else(|| TaskError::AgentUnavailable(agent_id.to_owned()))
}

/// The text of the last assistant message: its text parts, in order.
pub(crate) fn last_assistant_text(items: &[TimelineItem]) -> Option<String> {
    let last = items.iter().rev().find(|item| {
        item.role == TimelineRole::Assistant
            && matches!(&item.part, AgentPart::Text { text } if !text.trim().is_empty())
    })?;
    let text: Vec<&str> = items
        .iter()
        .filter(|item| item.message_id == last.message_id)
        .filter_map(|item| match &item.part {
            AgentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    Some(text.join("\n"))
}

/// One timeline item, reduced to what a calling model needs.
pub(crate) fn compact_item(item: &TimelineItem) -> Value {
    let role = serde_json::to_value(item.role).unwrap_or_default();
    let (kind, text) = match &item.part {
        AgentPart::Text { text } => ("text", text.clone()),
        AgentPart::Reasoning { .. } => ("reasoning", String::new()),
        AgentPart::Tool(call) => (
            "tool",
            format!(
                "{} [{}]",
                call.title.as_deref().unwrap_or(&call.name),
                serde_json::to_value(call.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
            ),
        ),
        AgentPart::Diff { file, .. } => ("diff", file.clone()),
        AgentPart::Status { text } => ("status", text.clone()),
        _ => ("other", String::new()),
    };
    // Never a marker: a parent agent reading it would quote it.
    let text = super::transcript::without_any_marker(&text);
    json!({ "role": role, "type": kind, "text": clip(&text, 2000) })
}

/// Find `id` in an agent's model catalog: either `<provider_id>/<model_id>`
/// or a bare `<model_id>`. The qualified spelling wins, so a model id that
/// two providers share can still be pinned to one of them; a bare id takes
/// the first provider the catalog lists.
///
/// `variant` is kept as is when the model lists no variants (the catalog says
/// nothing against it) and must be one of them otherwise. The outer `None` is
/// an unknown model, the inner `Err` an unknown variant.
fn resolve_model(
    models: &[ModelInfo],
    id: &str,
    variant: Option<&str>,
) -> Option<Result<ModelRef, (String, String)>> {
    let qualified = id.split_once('/').and_then(|(provider, model_id)| {
        models
            .iter()
            .find(|model| model.provider_id == provider && model.id == model_id)
    });
    qualified
        .or_else(|| models.iter().find(|model| model.id == id))
        .map(|found| {
            if let (Some(wanted), Some(listed)) = (variant, found.variants.as_deref()) {
                if !listed.is_empty() && !listed.iter().any(|v| v.id == wanted) {
                    let offered: Vec<&str> = listed.iter().map(|v| v.id.as_str()).collect();
                    return Err((wanted.to_owned(), offered.join(", ")));
                }
            }
            Ok(ModelRef {
                provider_id: found.provider_id.clone(),
                model_id: found.id.clone(),
                variant: variant.map(str::to_owned),
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::domain::{AgentPart, AgentSessionId, TimelineRole};
    use crate::tasks::domain::PrincipalKind;
    use crate::test_support::{test_device, test_state, FakeAgent};

    const ASID: &str = "opencode_new";

    #[test]
    fn shared_launch_fallback_is_reported_without_claiming_native_control() {
        let mut steps = crate::agents::tasks::StepLog::new();
        steps.skipped("env", "unrelated");
        assert!(shared_session_fallback_note(&steps.value()).is_none());
        steps.skipped(
            "shared_session",
            "per-invocation folder trust is unavailable",
        );
        assert_eq!(
            shared_session_fallback_note(&steps.value()).as_deref(),
            Some("Started in terminal mode: per-invocation folder trust is unavailable. Continue in the linked terminal.")
        );
        let mut shared = crate::agents::tasks::StepLog::new();
        shared.ok("shared_session", json!({"transport":"local_shared_daemon"}));
        assert!(shared_session_fallback_note(&shared.value()).is_none());
    }

    fn assistant(id: &str) -> TaskPrincipal {
        TaskPrincipal {
            kind: PrincipalKind::Assistant,
            id: id.into(),
        }
    }

    fn device() -> TaskPrincipal {
        TaskPrincipal {
            kind: PrincipalKind::Device,
            id: "phone".into(),
        }
    }

    fn request(machine: Option<&str>) -> DelegateRequest {
        DelegateRequest {
            goal: "fix the build".into(),
            cwd: Some("/work".into()),
            agent_id: None,
            mode: None,
            model: None,
            variant: None,
            machine: machine.map(str::to_owned),
            label: None,
            mentions: Vec::new(),
            client_ref: None,
            session_id: None,
            attachments: Vec::new(),
        }
    }

    fn reply(message: &str, text: &str) -> TimelineItem {
        TimelineItem {
            id: uuid::Uuid::new_v4().to_string(),
            message_id: message.into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text { text: text.into() },
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
        }
    }

    fn status(status: AgentSessionStatus) -> AgentDomainEvent {
        AgentDomainEvent::StatusChanged {
            asid: AgentSessionId(ASID.into()),
            status,
            error: None,
            seq: 0,
        }
    }

    fn permission_request(asid: &str) -> crate::agents::domain::PermissionRequest {
        serde_json::from_value(json!({
            "id": "perm1", "asid": asid, "action": "bash", "resources": [],
            "prompt": "run it?", "options": []
        }))
        .unwrap()
    }

    fn permission(asid: &str) -> AgentDomainEvent {
        AgentDomainEvent::PermissionPending {
            asid: AgentSessionId(asid.into()),
            request: permission_request(asid),
            seq: 0,
        }
    }

    fn resolved() -> AgentDomainEvent {
        AgentDomainEvent::PermissionResolved {
            asid: AgentSessionId(ASID.into()),
            request_id: "perm1".into(),
            seq: 0,
        }
    }

    /// A model provider's refusal reads as a sentence; any other error keeps
    /// its own words.
    #[test]
    fn a_provider_failure_is_worded_for_a_person() {
        let error = |name: &str, message: &str, status: Option<u16>| AgentErrorInfo {
            name: name.into(),
            message: message.into(),
            status,
        };
        for (error, worded) in [
            (
                error("provider.auth", "Request failed: 403", None),
                Some("OpenCode could not reach its model provider (HTTP 403)"),
            ),
            (
                error("ProviderAuthError", "bad key", Some(401)),
                Some("OpenCode could not reach its model provider (HTTP 401)"),
            ),
            (
                error("provider.rate_limit", "Request failed: 429", None),
                Some("OpenCode's model provider refused the request (429)"),
            ),
            (
                error("APIError", "https://api.example:443 answered 503", None),
                Some("OpenCode's model provider refused the request (503)"),
            ),
            (error("UnknownError", "boom", None), None),
        ] {
            assert_eq!(
                provider_failure("OpenCode", &error).as_deref(),
                worded,
                "{error:?}"
            );
        }
    }

    #[test]
    fn typed_fallback_requires_an_explicit_failure_and_no_delivery_attempt() {
        use crate::agents::session_routes::{AgentLaunch, PromptDelivery};
        for (step, code, delivery, retry) in [
            (
                "agent",
                "agent_start_failed",
                PromptDelivery::NotAttempted,
                true,
            ),
            (
                "agent",
                "agent_start_failed",
                PromptDelivery::Attempted,
                false,
            ),
            (
                "agent",
                "agent_start_timeout",
                PromptDelivery::NotAttempted,
                false,
            ),
            (
                "agent",
                "agent_not_found",
                PromptDelivery::NotAttempted,
                false,
            ),
            (
                "agent",
                "herdr_unavailable",
                PromptDelivery::NotAttempted,
                false,
            ),
            (
                "agent",
                "invalid_herdr_response",
                PromptDelivery::NotAttempted,
                false,
            ),
            (
                "pane",
                "agent_start_failed",
                PromptDelivery::NotAttempted,
                false,
            ),
            (
                "prompt",
                "agent_start_failed",
                PromptDelivery::NotAttempted,
                false,
            ),
        ] {
            let mut steps = crate::agents::tasks::StepLog::new();
            steps.failed(step, code, "x");
            let mut launch = AgentLaunch {
                payload: json!({ "agent_started": false, "prompt_submitted": false }),
                steps,
                prompt_delivery: delivery,
            };
            assert_eq!(
                may_retry_typed(&launch),
                retry,
                "{step}: {code}, {delivery:?}"
            );
            launch.payload = Value::Null;
            assert!(!may_retry_typed(&launch), "missing metadata: {code}");
        }
        for success in [false, true] {
            let mut steps = crate::agents::tasks::StepLog::new();
            if success {
                steps.ok("agent", json!({}));
                steps.ok("prompt", json!({ "via": "argv" }));
            }
            assert!(!may_retry_typed(&AgentLaunch {
                payload: json!({ "agent_started": false, "prompt_submitted": false }),
                steps,
                prompt_delivery: PromptDelivery::NotAttempted,
            }));
        }
    }

    async fn terminal_task(state: &AppState, pane_id: &str, goal: &str) -> Task {
        let now = now_ms();
        let task = Task {
            id: TaskId::new(),
            kind: TaskKind::Delegation,
            goal: goal.into(),
            cwd: None,
            binding: TaskBinding::Pane {
                session_id: state.config.sessions[0].id.clone(),
                pane_id: pane_id.into(),
                tab_id: None,
                shell_pid: None,
                agent_id: Some("claude".into()),
                agent_job: None,
                model: None,
                variant: None,
            },
            state: TaskState::Queued,
            summary: None,
            exit_code: None,
            created_by: assistant("a"),
            created_ms: now,
            updated_ms: now,
            seq: 0,
            parent: None,
            origin: TaskOrigin::Mcp,
            label: None,
            requester_name: None,
            session_title: None,
            mentions: Vec::new(),
            client_ref: None,
            requester_server_id: None,
            reopened_ms: None,
            reopen_count: 0,
        };
        state.tasks.insert_for_test(&task).await;
        state
            .tasks
            .move_to(&task.id, change(TaskState::Dispatching))
            .await
            .unwrap()
    }

    struct TaskStartHerdr {
        socket: crate::test_support::TestSocket,
        calls: Arc<std::sync::Mutex<Vec<Value>>>,
        serving: tokio::task::JoinHandle<()>,
    }

    impl TaskStartHerdr {
        fn start(first_error: Option<&'static str>) -> Self {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let socket = crate::test_support::TestSocket::new();
            let listener = tokio::net::UnixListener::bind(&socket.path).unwrap();
            let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = calls.clone();
            let serving = tokio::spawn(async move {
                let mut starts = 0;
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    recorded.lock().unwrap().push(request.clone());
                    let method = request["method"].as_str().unwrap();
                    let error = if method == "agent.start" {
                        starts += 1;
                        (starts == 1).then_some(first_error).flatten()
                    } else {
                        None
                    };
                    // A lost response cannot show whether the launch ran.
                    if error == Some("disconnect") {
                        continue;
                    }
                    let result = match method {
                        "tab.create" => {
                            json!({ "tab": { "tab_id": "w1:t1", "workspace_id": "w1", "active_pane_id": "w1:p1" } })
                        }
                        "pane.get" => {
                            json!({ "pane": { "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1" } })
                        }
                        "pane.list" => {
                            json!({ "panes": [{ "pane_id": "w1:p1", "workspace_id": "w1", "tab_id": "w1:t1" }] })
                        }
                        "pane.process_info" => {
                            json!({ "process_info": { "shell_pid": 4242, "foreground_processes": [{ "name": "sh", "argv0": "sh" }] } })
                        }
                        "pane.read" => json!({ "read": { "text": "$ ", "revision": 1 } }),
                        "agent.list" => json!({ "agents": [] }),
                        "ping" => json!({ "version": "0.9.0", "protocol": 17 }),
                        _ => json!({ "ok": true }),
                    };
                    let response = match error {
                        Some("malformed") => {
                            json!({ "id": request["id"], "result": { "agent": { "terminal_id": "t" } } })
                        }
                        Some(code) => {
                            json!({ "id": request["id"], "error": { "code": code, "message": "startup refused" } })
                        }
                        None => json!({ "id": request["id"], "result": result }),
                    };
                    let mut stream = reader.into_inner();
                    stream
                        .write_all(response.to_string().as_bytes())
                        .await
                        .unwrap();
                    stream.write_all(b"\n").await.unwrap();
                }
            });
            Self {
                socket,
                calls,
                serving,
            }
        }
    }

    impl Drop for TaskStartHerdr {
        fn drop(&mut self) {
            self.serving.abort();
        }
    }

    #[tokio::test]
    async fn terminal_start_retries_only_before_a_typed_goal_can_be_delivered() {
        for (error, starts, prompts, end) in [
            (Some("agent_start_failed"), 2, 1, TaskState::Working),
            (Some("agent_pane_busy"), 2, 1, TaskState::Working),
            (None, 1, 1, TaskState::Working),
            (Some("agent_start_timeout"), 1, 0, TaskState::Failed),
            (Some("agent_not_ready"), 1, 0, TaskState::Failed),
            (Some("disconnect"), 1, 0, TaskState::Failed),
            (Some("malformed"), 1, 0, TaskState::Failed),
        ] {
            let fake = TaskStartHerdr::start(error);
            let mut state = test_state("admin", Vec::new());
            state.config.agent_trust_folders = false;
            state.config.sessions[0].socket_path = fake.socket.path.to_string_lossy().into_owned();
            let session = state.config.sessions[0].clone();
            let placed =
                crate::agents::session_routes::place_agent(&state, &session, "claude", None, None)
                    .await
                    .unwrap();
            let goal = "apply the non-idempotent change once";
            let task = terminal_task(&state, &placed.place.pane_id, goal).await;
            start_terminal_agent(
                state.clone(),
                session,
                "claude".into(),
                placed,
                TerminalStart {
                    cwd: None,
                    goal: goal.into(),
                    model_args: Vec::new(),
                },
                task.clone(),
            )
            .await;
            assert_eq!(
                state.tasks.get_for_test(&task.id).await.state,
                end,
                "{error:?}"
            );
            let calls = fake.calls.lock().unwrap();
            let launches: Vec<_> = calls
                .iter()
                .filter(|call| call["method"] == "agent.start")
                .collect();
            let deliveries: Vec<_> = calls
                .iter()
                .filter(|call| call["method"] == "agent.prompt")
                .collect();
            assert_eq!(launches.len(), starts, "{error:?}: {calls:?}");
            assert_eq!(deliveries.len(), prompts, "{error:?}: {calls:?}");
            assert!(launches.iter().all(|call| !call["params"]["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|arg| arg.as_str().is_some_and(|arg| arg.contains(goal)))));
            if prompts > 0 {
                let typed_fallback = error == Some("agent_start_failed");
                let expected = if typed_fallback {
                    format!("{goal}\n\n{}", crate::mcp::inject::TASK_SENTENCE)
                } else {
                    goal.to_owned()
                };
                assert_eq!(deliveries[0]["params"]["text"], expected, "{error:?}");
                let args = launches.last().unwrap()["params"]["args"]
                    .as_array()
                    .unwrap();
                assert_eq!(
                    args.iter().any(|arg| arg
                        .as_str()
                        .is_some_and(|arg| arg.starts_with("--append-system-prompt"))),
                    !typed_fallback
                );
            }
        }
    }

    struct TaskTmux {
        dir: std::path::PathBuf,
        socket: std::path::PathBuf,
    }

    impl Drop for TaskTmux {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .arg("-S")
                .arg(&self.socket)
                .arg("kill-server")
                .output();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    #[ignore = "requires an isolated tmux server; runs only a fake shell agent"]
    async fn an_argv_goal_is_not_relaunched_after_success_and_a_rapid_exit() {
        let dir = crate::test_support::asset_test_dir("task-argv");
        let fake = TaskTmux {
            socket: dir.join("tmux.sock"),
            dir,
        };
        let created = std::process::Command::new("tmux")
            .arg("-S")
            .arg(&fake.socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "task-argv",
                "/bin/sh",
            ])
            .output()
            .unwrap();
        assert!(created.status.success(), "{created:?}");
        let mut state = test_state("admin", Vec::new());
        state.config.agent_trust_folders = false;
        state.config.sessions[0].backend = crate::backend::BackendKind::Tmux;
        state.config.sessions[0].socket_path = fake.socket.to_string_lossy().into_owned();
        state
            .config
            .agent_commands
            .insert("claude".into(), "sh".into());
        let effects = fake.dir.join("effects");
        let argv = fake.dir.join("argv");
        let release = fake.dir.join("release");
        // A non-idempotent side effect, then an exit. No AI process is run.
        let script = format!(
            "printf 'effect\\n' >> {}; printf '%s\\n' \"$@\" >> {}; while [ ! -f {} ]; do :; done",
            crate::terminal::shell_word::shell_word(&effects.to_string_lossy()),
            crate::terminal::shell_word::shell_word(&argv.to_string_lossy()),
            crate::terminal::shell_word::shell_word(&release.to_string_lossy()),
        );
        state
            .config
            .agent_launch_args
            .insert("claude".into(), vec!["-c".into(), script]);
        let session = state.config.sessions[0].clone();
        let placed =
            crate::agents::session_routes::place_agent(&state, &session, "claude", None, None)
                .await
                .unwrap();
        let goal = "apply the non-idempotent change once";
        let task = terminal_task(&state, &placed.place.pane_id, goal).await;
        let exit = tokio::spawn({
            let effects = effects.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !effects.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_millis(150)).await;
                std::fs::write(release, "exit").unwrap();
            }
        });
        start_terminal_agent(
            state.clone(),
            session,
            "claude".into(),
            placed,
            TerminalStart {
                cwd: None,
                goal: goal.into(),
                model_args: Vec::new(),
            },
            task.clone(),
        )
        .await;
        exit.await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(std::fs::read_to_string(effects).unwrap(), "effect\n");
        let argv = std::fs::read_to_string(argv).unwrap();
        assert!(argv.contains("--append-system-prompt"), "{argv}");
        assert_eq!(argv.lines().filter(|arg| *arg == goal).count(), 1, "{argv}");
        assert_eq!(
            state.tasks.get_for_test(&task.id).await.state,
            TaskState::Working
        );
        // The argv delivery disposition must survive a backend error too;
        // absence of a successful prompt step is not proof of non-delivery.
        state
            .config
            .agent_commands
            .insert("claude".into(), "muqun-nonexistent-task-test-agent".into());
        let placed = crate::agents::session_routes::place_agent(
            &state,
            &state.config.sessions[0],
            "claude",
            None,
            None,
        )
        .await
        .unwrap();
        let (failed, to_type) = crate::agents::session_routes::start_agent_in_place(
            &state,
            &state.config.sessions[0],
            "claude",
            None,
            placed,
            Some(goal),
            None,
        )
        .await;
        assert_eq!(
            failed.prompt_delivery,
            crate::agents::session_routes::PromptDelivery::Attempted
        );
        assert!(failed.steps.has_failure());
        assert!(to_type.is_none());
        assert!(!may_retry_typed(&failed));
    }

    /// A state with an OpenCode fake whose new session exists (idle, unless
    /// the fake already lists its sessions), and a task delegated to it by
    /// `assistant("a")`.
    async fn delegated(agent: FakeAgent) -> (AppState, TaskId) {
        let state = test_state("admin", vec![test_device("d1", "token")]);
        let agent = if agent.sessions.is_empty() {
            agent.with_sessions(&[(ASID, 1)])
        } else {
            agent
        };
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        (state, TaskId(view.task_id))
    }

    /// `wait` by the owner, with `events` published once it has subscribed.
    async fn wait_through(
        state: &AppState,
        id: &TaskId,
        events: Vec<AgentDomainEvent>,
    ) -> TaskView {
        let waiting = {
            let (state, id) = (state.clone(), id.clone());
            tokio::spawn(async move {
                state
                    .tasks
                    .wait(&state, &assistant("a"), &id, Duration::from_secs(2))
                    .await
                    .map(|waited| waited.task)
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        for event in events {
            state.agent_runtime.publish_for_test(event);
        }
        waiting.await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_new_user_turn_reopens_only_a_live_finished_session() {
        for (ended, live, fresh, expected) in [
            (TaskState::Done, true, true, true),
            (TaskState::Failed, true, true, true),
            (TaskState::Cancelled, true, true, false),
            (TaskState::Done, false, true, false),
            (TaskState::Done, true, false, false),
        ] {
            let agent = FakeAgent::new("opencode");
            let (state, id) = if live {
                delegated(agent).await
            } else {
                tracked(agent).await
            };
            let task = state
                .tasks
                .move_to(
                    &id,
                    TaskChange {
                        to: ended,
                        summary: Some("first result".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let mut user = reply("follow-up", "Please continue");
            user.role = TimelineRole::User;
            user.updated_ms = task.updated_ms + u64::from(fresh);
            assert_eq!(
                state.tasks.reopen_session(&state, &task, &[user]).await,
                expected
            );
            let now = state.tasks.task(&id).await.unwrap().unwrap();
            assert_eq!(now.state, if expected { TaskState::Working } else { ended });
            assert_eq!(now.summary.as_deref(), Some("first result"));
            assert_eq!(now.reopen_count, u64::from(expected));
            if expected {
                assert_eq!(now.reopened_ms, Some(task.updated_ms + 1));
                // A duplicate observer cannot reopen the same finish twice.
                assert!(state
                    .tasks
                    .reopen(&task, task.updated_ms + 1)
                    .await
                    .is_err());
                let done = state
                    .tasks
                    .move_to(
                        &id,
                        TaskChange {
                            to: TaskState::Done,
                            summary: Some("second result".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(done.id, task.id);
                assert_eq!(done.summary.as_deref(), Some("second result"));
                assert_eq!(done.reopen_count, 1);
            }
        }
    }

    #[tokio::test]
    async fn delegate_creates_a_session_and_a_working_task() {
        let agent = FakeAgent::new("opencode");
        let (calls, prompts) = (agent.calls.clone(), agent.prompts.clone());
        let (state, id) = delegated(agent).await;
        assert_eq!(*calls.lock().unwrap(), ["create_session", "send_prompt"]);
        // Asked for an explicit end; the goal is stored as given.
        let sent = ["fix the build"];
        assert_eq!(*prompts.lock().unwrap(), sent);
        let view = state
            .tasks
            .list_for(None, None, 10)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(view.task_id, id.0);
        assert_eq!(view.goal, "fix the build");
        assert_eq!(view.state, TaskState::Working);
        assert_eq!(view.kind, TaskKind::Delegation);
        assert_eq!(
            view.binding,
            TaskBinding::AgentSession {
                agent_id: "opencode".into(),
                asid: ASID.into(),
            }
        );
        let routed = state.agent_runtime.manager_for_session(ASID).await;
        assert_eq!(routed.map(|m| m.agent().kind()), Some("opencode"));
    }

    fn model(provider_id: &str, id: &str) -> ModelInfo {
        serde_json::from_value(json!({ "id": id, "name": id, "provider_id": provider_id })).unwrap()
    }

    #[test]
    fn a_model_is_named_by_its_id_or_by_provider_and_id() {
        let models = [
            model("openrouter", "claude-haiku-5-5"),
            model("anthropic", "claude-haiku-5-5"),
            model("openrouter", "anthropic/claude-opus"),
        ];
        let bare = resolve_model(&models, "claude-haiku-5-5", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (bare.provider_id.as_str(), bare.model_id.as_str()),
            ("openrouter", "claude-haiku-5-5")
        );
        let pinned = resolve_model(&models, "anthropic/claude-haiku-5-5", None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (pinned.provider_id.as_str(), pinned.model_id.as_str()),
            ("anthropic", "claude-haiku-5-5")
        );
        // An id that itself carries a slash still matches as a bare id.
        let slashed = resolve_model(&models, "anthropic/claude-opus", None)
            .unwrap()
            .unwrap();
        assert_eq!(slashed.provider_id, "openrouter");
        assert_eq!(resolve_model(&models, "gpt-9", None), None);
        assert_eq!(resolve_model(&models, "anthropic/gpt-9", None), None);
    }

    #[tokio::test]
    async fn delegate_with_an_unknown_model_creates_nothing() {
        let state = test_state("admin", Vec::new());
        let mut agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        agent.catalog.models = vec![model("anthropic", "claude-haiku-5-5")];
        let calls = agent.calls.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let mut unknown = request(None);
        unknown.model = Some("gpt-9".into());
        let error = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), unknown)
            .await
            .unwrap_err();
        assert!(matches!(error, TaskError::UnknownModel(ref id) if id == "gpt-9"));
        assert!(calls.lock().unwrap().is_empty());
        assert!(state
            .tasks
            .list_for(None, None, 10)
            .await
            .unwrap()
            .is_empty());

        let mut known = request(None);
        known.model = Some("anthropic/claude-haiku-5-5".into());
        state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), known)
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["create_session", "send_prompt"]);
    }

    #[tokio::test]
    async fn every_recorded_change_is_published() {
        let state = test_state("admin", Vec::new());
        let agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let mut events = state.task_events.subscribe();
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        let mut states = Vec::new();
        while let Ok(event) = events.try_recv() {
            assert_eq!(event.event_name(), "task.updated");
            assert!(event.seq() > 0);
            let TaskDomainEvent::Updated { task, .. } = event;
            assert_eq!(task.task_id, view.task_id);
            states.push(task.state);
        }
        assert_eq!(
            states,
            [
                TaskState::Queued,
                TaskState::Dispatching,
                TaskState::Working
            ]
        );
    }

    #[tokio::test]
    async fn wait_needs_busy_before_idle() {
        let agent = FakeAgent::new("opencode")
            .with_timeline(vec![reply("m1", "looking"), reply("m2", "all green")]);
        let (state, id) = delegated(agent).await;
        // Idle from the start and never busy: the prompt is still being
        // picked up, not done.
        let waited = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(1))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(waited.state, TaskState::Working);
        let idle_only = wait_through(&state, &id, vec![status(AgentSessionStatus::Idle)]).await;
        assert_eq!(idle_only.state, TaskState::Working);

        let busy_then_idle = vec![
            status(AgentSessionStatus::Busy),
            status(AgentSessionStatus::Idle),
        ];
        let done = wait_through(&state, &id, busy_then_idle).await;
        assert_eq!(done.state, TaskState::IdleUnconfirmed);
        assert_eq!(done.summary.as_deref(), Some("all green"));
    }

    #[tokio::test]
    async fn a_session_idle_since_the_prompt_finished_before_the_wait() {
        let mut idle_after = crate::test_support::fake_session(ASID, 1);
        idle_after.time_idle = Some(now_ms() + 60_000);
        let mut agent = FakeAgent::new("opencode").with_timeline(vec![reply("m1", "ok")]);
        agent.sessions = vec![idle_after];
        let state = test_state("admin", Vec::new());
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        let id = TaskId(view.task_id);
        let done = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(1))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(done.state, TaskState::IdleUnconfirmed);
        assert_eq!(done.summary.as_deref(), Some("ok"));
    }

    /// A waiter sees a request raised while it waits (its own session's,
    /// not another's), and one raised before it began.
    #[tokio::test]
    async fn a_pending_request_makes_a_waiter_answer_waiting() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let pending = vec![permission("someone_else"), permission(ASID)];
        let waited = wait_through(&state, &id, pending).await;
        assert_eq!(waited.state, TaskState::Waiting);

        let agent = FakeAgent::new("opencode");
        agent
            .permissions
            .lock()
            .unwrap()
            .push(permission_request(ASID));
        let (state, id) = delegated(agent).await;
        let waited = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(5))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(waited.state, TaskState::Waiting);
    }

    /// Already waiting with the request still pending: answer at once.
    #[tokio::test]
    async fn a_waiting_task_with_its_request_still_pending_answers_at_once() {
        let agent = FakeAgent::new("opencode");
        agent
            .permissions
            .lock()
            .unwrap()
            .push(permission_request(ASID));
        let (state, id) = delegated(agent).await;
        let first = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(45))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(first.state, TaskState::Waiting);
        let started = std::time::Instant::now();
        let again = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(45))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(again.state, TaskState::Waiting);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
    }

    /// Answered while nobody waited: running with nothing pending is working.
    #[tokio::test]
    async fn a_waiting_task_whose_session_runs_again_is_working() {
        let mut busy = crate::test_support::fake_session(ASID, 1);
        busy.status = AgentSessionStatus::Busy;
        let mut agent = FakeAgent::new("opencode");
        agent.sessions = vec![busy];
        let state = test_state("admin", Vec::new());
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        let id = TaskId(view.task_id);
        let waited = wait_through(&state, &id, vec![permission(ASID)]).await;
        assert_eq!(waited.state, TaskState::Waiting);
        let again = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(1))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(again.state, TaskState::Working);
    }

    /// Two waiters on one task: the one that loses the race to record the
    /// change answers with what the other recorded.
    #[tokio::test]
    async fn concurrent_waiters_both_see_the_change() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let spawn = || {
            let (state, id) = (state.clone(), id.clone());
            tokio::spawn(async move {
                state
                    .tasks
                    .wait(&state, &assistant("a"), &id, Duration::from_secs(2))
                    .await
                    .map(|waited| waited.task)
            })
        };
        let (first, second) = (spawn(), spawn());
        tokio::time::sleep(Duration::from_millis(200)).await;
        state.agent_runtime.publish_for_test(permission(ASID));
        for waiter in [first, second] {
            let view = waiter.await.unwrap().unwrap();
            assert_eq!(view.state, TaskState::Waiting);
        }
    }

    #[tokio::test]
    async fn another_principal_cannot_read_or_cancel() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let other = assistant("b");
        let read = state
            .tasks
            .read(&state, &other, &id, ReadWhat::Summary, None)
            .await;
        assert!(matches!(read, Err(TaskError::NotYours)), "{read:?}");
        let cancel = state.tasks.cancel(&state, &other, &id).await;
        assert!(matches!(cancel, Err(TaskError::NotYours)), "{cancel:?}");
        let wait = state
            .tasks
            .wait(&state, &other, &id, Duration::from_secs(1))
            .await
            .map(|waited| waited.task);
        assert!(matches!(wait, Err(TaskError::NotYours)), "{wait:?}");
        assert!(state
            .tasks
            .list_for(Some(&other), None, 10)
            .await
            .unwrap()
            .is_empty());

        let summary = state
            .tasks
            .read(&state, &device(), &id, ReadWhat::Summary, None)
            .await
            .unwrap();
        assert_eq!(summary["task_id"], id.0);
        assert_eq!(summary["state"], "working");
        let all = state
            .tasks
            .list_for(Some(&device()), None, 10)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
        let missing = TaskId("t_missing".into());
        let missing = state
            .tasks
            .read(&state, &device(), &missing, ReadWhat::Summary, None)
            .await;
        assert!(matches!(missing, Err(TaskError::NotFound)), "{missing:?}");
    }

    #[tokio::test]
    async fn unknown_machine_is_refused() {
        let state = test_state("admin", Vec::new());
        let agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let refused = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(Some("mac")))
            .await;
        assert!(
            matches!(&refused, Err(TaskError::UnknownMachine { wanted, .. }) if wanted == "mac"),
            "{refused:?}"
        );
        for machine in ["self", "server-1"] {
            let view = state
                .tasks
                .delegate(
                    &state,
                    Requested::mcp(assistant("a")),
                    request(Some(machine)),
                )
                .await
                .unwrap();
            assert_eq!(view.state, TaskState::Working);
        }
    }

    #[tokio::test]
    async fn forwarding_refuses_unknown_peer_and_broken_ancestry() {
        use crate::tasks::domain::ForwardingContext;
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let mut by = Requested::mcp(assistant("a"));
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::LineageUnresolved)
        ));
        by.forwarding = ForwardingContext::Resolved;
        assert!(state.tasks.allow_forwarding(&by, "mini").await.is_ok());
        by.parent = Some(TaskId("missing".into()));
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::LineageUnresolved)
        ));
        // A child created locally while its caller was unknown cannot wash
        // that missing lineage into a known local root on its next call.
        by.parent = Some(id.clone());
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::LineageUnresolved)
        ));
        let mut root = state.tasks.get_for_test(&id).await;
        root.id = TaskId::new();
        root.origin = TaskOrigin::Device;
        root.created_by = device();
        let local = state.tasks.insert_for_test(&root).await;
        by.parent = Some(local.clone());
        assert!(state.tasks.allow_forwarding(&by, "mini").await.is_ok());
        root.id = TaskId::new();
        root.origin = TaskOrigin::Peer;
        let peer = state.tasks.insert_for_test(&root).await;
        by.parent = Some(peer);
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::DelegationLoop(_))
        ));
        root.id = TaskId::new();
        root.origin = TaskOrigin::Device;
        root.parent = Some(root.id.clone());
        by.parent = Some(state.tasks.insert_for_test(&root).await);
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::LineageUnresolved)
        ));
        by.parent = Some(local);
        state
            .db
            .call(|db| {
                db.execute_batch("DROP TABLE tasks")?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            state.tasks.allow_forwarding(&by, "mini").await,
            Err(TaskError::LineageUnresolved)
        ));
    }

    /// A store that refuses every move to `refuse`.
    struct Refusing {
        inner: Arc<dyn TaskStore>,
        refuse: TaskState,
    }

    impl TaskStore for Refusing {
        fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task> {
            self.inner.insert(task)
        }
        fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
            self.inner.get(id)
        }
        fn latest_device_cwd<'a>(
            &'a self,
            device: &'a TaskPrincipal,
        ) -> TaskFuture<'a, Option<String>> {
            self.inner.latest_device_cwd(device)
        }
        fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>> {
            self.inner.list(filter)
        }
        fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task> {
            if change.to == self.refuse {
                return Box::pin(async { Err(anyhow::anyhow!("the store is full")) });
            }
            self.inner.transition(id, change)
        }
        fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
            self.inner.events(id)
        }
        fn child_counts<'a>(
            &'a self,
            ids: Vec<TaskId>,
        ) -> TaskFuture<'a, HashMap<TaskId, ChildCounts>> {
            self.inner.child_counts(ids)
        }
        fn purge_ended<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize> {
            self.inner.purge_ended(before_ms)
        }
    }

    /// The store fails once the session exists: the session is interrupted
    /// and the task does not stay half-dispatched.
    #[tokio::test]
    async fn a_store_failure_after_the_session_exists_interrupts_it() {
        let mut state = test_state("admin", Vec::new());
        let inner: Arc<dyn TaskStore> = Arc::new(crate::tasks::store_sqlite::SqliteTaskStore::new(
            state.db.clone(),
        ));
        state.tasks = Arc::new(TaskService::new(
            Arc::new(Refusing {
                inner: inner.clone(),
                refuse: TaskState::Dispatching,
            }),
            state.task_events.clone(),
            state.config.server_id.clone(),
            state.peers.presence.clone(),
        ));
        let agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        let calls = agent.calls.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let refused = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await;
        assert!(matches!(refused, Err(TaskError::Store(_))), "{refused:?}");
        assert_eq!(*calls.lock().unwrap(), ["create_session", "interrupt"]);
        // Queued is all the store took; it never claims a live session.
        let tasks = inner.list(TaskFilter::default()).await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, TaskState::Queued);
        assert_eq!(tasks[0].binding, TaskBinding::None);
    }

    /// The session cannot be created: the recorded task fails and nothing
    /// is left to interrupt.
    #[tokio::test]
    async fn a_session_that_cannot_be_created_fails_the_task() {
        let state = test_state("admin", Vec::new());
        let agent = FakeAgent::new("opencode").failing();
        let calls = agent.calls.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let refused = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await;
        assert!(matches!(refused, Err(TaskError::Agent(_))), "{refused:?}");
        assert!(calls.lock().unwrap().is_empty());
        let task = state
            .tasks
            .list_for(None, None, 10)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.goal, "fix the build");
        let summary = task.summary.expect("the failure is the summary");
        assert!(
            summary.starts_with("agent: ") && !summary.contains('\n'),
            "{summary}"
        );
    }

    #[test]
    fn the_way_to_a_state_passes_the_states_between() {
        use TaskState::*;
        assert_eq!(steps_between(Queued, Failed), [Dispatching]);
        assert!(steps_between(Waiting, Failed).is_empty(), "a direct move");
        assert_eq!(steps_between(Dispatching, Done), [Working]);
        assert_eq!(steps_between(Queued, Done), [Dispatching, Working]);
        assert!(steps_between(Working, Done).is_empty());
        assert!(steps_between(Done, Working).is_empty(), "terminal stays");
    }

    #[tokio::test]
    async fn no_attached_agent_is_agent_unavailable() {
        let state = test_state("admin", Vec::new());
        let refused = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await;
        assert!(
            matches!(refused, Err(TaskError::AgentUnavailable(_))),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn cancel_interrupts_and_marks_cancelled() {
        let agent = FakeAgent::new("opencode");
        let calls = agent.calls.clone();
        let (state, id) = delegated(agent).await;
        let cancelled = state
            .tasks
            .cancel(&state, &assistant("a"), &id)
            .await
            .unwrap();
        assert_eq!(cancelled.state, TaskState::Cancelled);
        assert_eq!(
            calls.lock().unwrap().last().map(String::as_str),
            Some("interrupt")
        );
        // A finished task answers at once.
        let waited = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(30))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(waited.state, TaskState::Cancelled);

        // An agent that no longer has the session cannot be interrupted, and
        // the task is cancelled all the same.
        let agent = FakeAgent::new("opencode");
        let calls = agent.calls.clone();
        let (state, id) = tracked(agent).await;
        let cancelled = state.tasks.cancel(&state, &assistant("a"), &id).await;
        assert_eq!(cancelled.unwrap().state, TaskState::Cancelled);
        assert!(!calls.lock().unwrap().iter().any(|call| call == "interrupt"));
    }

    #[tokio::test]
    async fn the_timeline_reads_after_the_cursor() {
        let mut first = reply("m1", "first");
        first.seq = 3;
        let mut second = reply("m2", "second");
        second.seq = 7;
        let agent = FakeAgent::new("opencode").with_timeline(vec![first, second]);
        let (state, id) = delegated(agent).await;
        let page = state
            .tasks
            .read(&state, &assistant("a"), &id, ReadWhat::Timeline, Some(3))
            .await
            .unwrap();
        assert_eq!(
            page["items"],
            json!([{"role": "assistant", "type": "text", "text": "second"}])
        );
        assert_eq!(page["cursor"], 7);
    }

    #[test]
    fn the_summary_is_the_last_assistant_message_text() {
        let mut user = reply("u1", "do it");
        user.role = TimelineRole::User;
        let mut thinking = reply("a2", "");
        thinking.part = AgentPart::Reasoning {
            text: "hmm".into(),
            duration_ms: None,
        };
        let items = vec![
            user,
            reply("a1", "first"),
            reply("a2", "done:"),
            thinking,
            reply("a2", "all green"),
        ];
        assert_eq!(
            last_assistant_text(&items).as_deref(),
            Some("done:\nall green")
        );
        assert_eq!(last_assistant_text(&items[..1]), None);
        assert_eq!(
            compact_item(&items[1]),
            json!({"role": "assistant", "type": "text", "text": "first"})
        );
        // A child's marker never reaches the agent reading it.
        assert_eq!(
            compact_item(&reply("a3", "Built.\nMUQUN_TASK_DONE: built")),
            json!({"role": "assistant", "type": "text", "text": "Built."})
        );
    }

    /// A delegation whose session finished its turn after the task began.
    fn finished_agent(timeline: Vec<TimelineItem>) -> FakeAgent {
        let mut session = crate::test_support::fake_session(ASID, 1);
        session.time_idle = Some(now_ms() + 60_000);
        let mut agent = FakeAgent::new("opencode").with_timeline(timeline);
        agent.sessions = vec![session];
        agent
    }

    #[tokio::test]
    async fn a_finished_turn_pauses_the_task_with_its_result_line_as_it_happens() {
        let long = format!("Fixed it.\n\nThe build was {}", "very ".repeat(80));
        let (state, id) = tracked(finished_agent(vec![reply("m1", &long)])).await;
        let mut updates = state.task_events.subscribe();
        // Not about this session, or not a settling event: nothing moves.
        assert!(
            !state
                .tasks
                .follow(&state, &status(AgentSessionStatus::Busy))
                .await
        );
        let elsewhere = AgentDomainEvent::StatusChanged {
            asid: AgentSessionId("ses_other".into()),
            status: AgentSessionStatus::Idle,
            error: None,
            seq: 0,
        };
        assert!(!state.tasks.follow(&state, &elsewhere).await);

        assert!(
            state
                .tasks
                .follow(&state, &status(AgentSessionStatus::Idle))
                .await
        );
        let task = state.tasks.get_for_test(&id).await;
        let summary = task.summary.unwrap();
        assert!(
            summary.starts_with("Fixed it. The build was very"),
            "{summary}"
        );
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= SUMMARY_CHARS && summary.ends_with('…'));
        assert_eq!(task.exit_code, None);
        let TaskDomainEvent::Updated { task: frame, .. } = updates.try_recv().unwrap();
        assert_eq!(frame.state, TaskState::IdleUnconfirmed);
        assert_eq!(frame.summary.as_deref(), Some(summary.as_str()));
        let sources: Vec<String> = state
            .tasks
            .events_for_test(&id)
            .await
            .into_iter()
            .map(|e| e.source)
            .collect();
        assert_eq!(sources.last().unwrap(), "task_follower");

        // A waiter still gets the whole message.
        let waited = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(waited.answer.as_deref(), Some(long.as_str()));
        assert!(
            !state
                .tasks
                .follow(&state, &status(AgentSessionStatus::Idle))
                .await
        );
    }

    /// `agent` attached, and a Working delegation by `assistant("a")` bound
    /// to its session [`ASID`], recorded without going through `delegate`.
    async fn tracked(agent: FakeAgent) -> (AppState, TaskId) {
        let state = test_state("admin", Vec::new());
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let now = now_ms();
        let id = state
            .tasks
            .insert_for_test(&Task {
                id: TaskId::new(),
                kind: TaskKind::Delegation,
                goal: "fix the build".into(),
                cwd: None,
                binding: TaskBinding::AgentSession {
                    agent_id: "opencode".into(),
                    asid: ASID.into(),
                },
                state: TaskState::Queued,
                summary: None,
                exit_code: None,
                created_by: assistant("a"),
                created_ms: now,
                updated_ms: now,
                seq: 0,
                parent: None,
                origin: TaskOrigin::Mcp,
                label: None,
                requester_name: None,
                session_title: None,
                mentions: Vec::new(),
                client_ref: None,
                requester_server_id: None,
                reopened_ms: None,
                reopen_count: 0,
            })
            .await;
        state
            .tasks
            .move_to(&id, change(TaskState::Working))
            .await
            .unwrap();
        (state, id)
    }

    /// The turn ended before `delegate` recorded Working (the follower could
    /// not settle a task that was still dispatching): `delegate` settles it.
    #[tokio::test]
    async fn a_turn_that_ends_during_dispatch_is_settled_when_delegate_returns() {
        let state = test_state("admin", Vec::new());
        let agent = finished_agent(vec![reply("m1", "quick")]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        assert_eq!(view.state, TaskState::IdleUnconfirmed);
        assert_eq!(view.summary.as_deref(), Some("quick"));
    }

    /// A store that, on the move to Working, first records `Failed` as the
    /// follower would on the session's failure event.
    struct Racing {
        inner: Arc<dyn TaskStore>,
    }

    impl TaskStore for Racing {
        fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task> {
            self.inner.insert(task)
        }
        fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
            self.inner.get(id)
        }
        fn latest_device_cwd<'a>(
            &'a self,
            device: &'a TaskPrincipal,
        ) -> TaskFuture<'a, Option<String>> {
            self.inner.latest_device_cwd(device)
        }
        fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>> {
            self.inner.list(filter)
        }
        fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task> {
            Box::pin(async move {
                if change.to == TaskState::Working {
                    let failed = TaskChange {
                        to: TaskState::Failed,
                        source: FOLLOWER.into(),
                        summary: Some("ProviderError: overloaded".into()),
                        ..Default::default()
                    };
                    self.inner.transition(id, failed).await?;
                }
                self.inner.transition(id, change).await
            })
        }
        fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
            self.inner.events(id)
        }
        fn child_counts<'a>(
            &'a self,
            ids: Vec<TaskId>,
        ) -> TaskFuture<'a, HashMap<TaskId, ChildCounts>> {
            self.inner.child_counts(ids)
        }
        fn purge_ended<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize> {
            self.inner.purge_ended(before_ms)
        }
    }

    /// The session fails while the prompt is being sent and the follower
    /// records it first: `delegate` answers the failed task, does not call it
    /// an error, and does not interrupt a session that already stopped.
    #[tokio::test]
    async fn a_session_failing_during_dispatch_is_answered_not_an_error() {
        let mut state = test_state("admin", Vec::new());
        let inner: Arc<dyn TaskStore> = Arc::new(crate::tasks::store_sqlite::SqliteTaskStore::new(
            state.db.clone(),
        ));
        state.tasks = Arc::new(TaskService::new(
            Arc::new(Racing { inner }),
            state.task_events.clone(),
            state.config.server_id.clone(),
            state.peers.presence.clone(),
        ));
        let agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        let calls = agent.calls.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        assert_eq!(view.state, TaskState::Failed);
        assert_eq!(view.summary.as_deref(), Some("ProviderError: overloaded"));
        assert!(matches!(view.binding, TaskBinding::AgentSession { .. }));
        assert_eq!(*calls.lock().unwrap(), ["create_session", "send_prompt"]);
    }

    /// A session delegation is done only on an explicit end: the marker in
    /// the agent's last message, or `tasks.complete`. A turn that just stops
    /// is a pause; a new turn takes it back to working.
    #[tokio::test]
    async fn a_session_task_is_done_only_when_the_agent_says_so() {
        enum Input {
            Event(Box<AgentDomainEvent>),
            Complete(&'static str, Outcome),
            /// Blocked by the marker of a turn that has ended.
            Marked(&'static str),
            /// A tool call after the agent's last message.
            ToolAfter,
        }
        use Input::*;
        use TaskState::*;
        let idle = || Event(Box::new(status(AgentSessionStatus::Idle)));
        let busy = || Event(Box::new(status(AgentSessionStatus::Busy)));
        let blocked = || Complete("notes.md does not exist", Outcome::Blocked);
        let form = || {
            Event(Box::new(AgentDomainEvent::FormPending {
                asid: AgentSessionId(ASID.into()),
                request: serde_json::from_value(json!({
                    "id": "form1", "asid": ASID, "title": "Pick a branch", "fields": []
                }))
                .unwrap(),
                seq: 0,
            }))
        };
        let marked = "Done.\n\nMUQUN_TASK_DONE: the build is green";
        let rows = [
            (
                "a turn that stops",
                Working,
                "Shall I update the docs too?",
                vec![idle()],
                IdleUnconfirmed,
                Some("Shall I update the docs too?"),
            ),
            (
                "the marker",
                Working,
                marked,
                vec![idle()],
                Done,
                Some("the build is green"),
            ),
            (
                "the marker after the agent's words on its line",
                Working,
                "Hello. How can I help?MUQUN_TASK_DONE: Greeted the user",
                vec![idle()],
                Done,
                Some("Greeted the user"),
            ),
            (
                "a hint never carries marker text",
                Working,
                "Shall I go on? `MUQUN_TASK_DONE: <one-line summary>`",
                vec![idle()],
                IdleUnconfirmed,
                Some("Shall I go on?"),
            ),
            (
                "the marker's placeholder",
                Working,
                "MUQUN_TASK_DONE: <one-line summary>",
                vec![idle()],
                IdleUnconfirmed,
                None,
            ),
            (
                "a permission request: its prompt, whatever the agent said before",
                Working,
                "Let me clean the build dir.",
                vec![Event(Box::new(permission(ASID)))],
                Waiting,
                Some("run it?"),
            ),
            (
                "a question form: the agent's own question",
                Working,
                "I need one detail.\nWhich branch should I use?",
                vec![form()],
                Waiting,
                Some("Which branch should I use?"),
            ),
            (
                "a question form after a tool call: its title",
                Working,
                "Let me look at the branches.",
                vec![ToolAfter, form()],
                Waiting,
                Some("Pick a branch"),
            ),
            (
                "a new turn after a pause",
                IdleUnconfirmed,
                "",
                vec![busy()],
                Working,
                None,
            ),
            (
                "the marker after a pause",
                IdleUnconfirmed,
                marked,
                vec![idle()],
                Done,
                Some("the build is green"),
            ),
            (
                "tasks.complete",
                Working,
                "",
                vec![Complete("tests pass", Outcome::Done)],
                Done,
                Some("tests pass"),
            ),
            (
                "a question answered",
                Waiting,
                "",
                vec![Event(Box::new(resolved()))],
                Working,
                None,
            ),
            (
                "tasks.complete while waiting",
                Waiting,
                "",
                vec![Complete("tests pass", Outcome::Done)],
                Done,
                Some("tests pass"),
            ),
            (
                "the blocked marker",
                Working,
                "MUQUN_TASK_BLOCKED: notes.md does not exist",
                vec![idle()],
                Waiting,
                Some("notes.md does not exist"),
            ),
            (
                "tasks.complete blocked, then its turn ends without the marker",
                Working,
                "I could not find notes.md.",
                vec![blocked(), idle()],
                Waiting,
                Some("notes.md does not exist"),
            ),
            (
                "tasks.complete blocked: the rest of its own turn is no answer",
                Working,
                "",
                vec![blocked(), busy()],
                Waiting,
                None,
            ),
            (
                "tasks.complete blocked: a turn after it ended (the user answered) works",
                Working,
                "",
                vec![blocked(), idle(), busy()],
                Working,
                None,
            ),
            (
                "marker blocked: the session running again (the user answered) works",
                Working,
                "",
                vec![Marked("notes.md does not exist"), busy()],
                Working,
                None,
            ),
            (
                "waiting on a permission: an old blocked line does not re-block it",
                Working,
                "MUQUN_TASK_BLOCKED: notes.md does not exist",
                vec![Event(Box::new(permission(ASID))), idle()],
                Waiting,
                Some("run it?"),
            ),
            (
                "marker blocked, then blocked for a new reason",
                Working,
                "MUQUN_TASK_BLOCKED: the config is missing too",
                vec![Marked("notes.md does not exist"), idle()],
                Waiting,
                Some("the config is missing too"),
            ),
            (
                "tasks.complete blocked, then done",
                IdleUnconfirmed,
                "",
                vec![blocked(), Complete("tests pass", Outcome::Done)],
                Done,
                Some("tests pass"),
            ),
        ];
        for (name, from, last, inputs, to, summary) in rows {
            let mut timeline = if last.is_empty() {
                Vec::new()
            } else {
                vec![reply("m1", last)]
            };
            if inputs.iter().any(|input| matches!(input, ToolAfter)) {
                let mut call = reply("m2", "");
                call.part = AgentPart::Tool(
                    serde_json::from_value(json!({
                        "id": "c1", "name": "bash", "input": { "command": "git branch" },
                        "state": "completed", "status": "completed"
                    }))
                    .unwrap(),
                );
                timeline.push(call);
            }
            let (state, id) = tracked(finished_agent(timeline)).await;
            if from != Working {
                state.tasks.move_to(&id, change(from)).await.unwrap();
            }
            for input in inputs {
                match input {
                    Event(event) => {
                        state.tasks.follow(&state, &event).await;
                    }
                    Complete(text, outcome) => {
                        state.tasks.complete(&id, text, outcome).await.unwrap();
                    }
                    Marked(reason) => {
                        let block = TaskChange {
                            summary: Some(reason.into()),
                            note: Some(reason.into()),
                            source: pane_agent::MARKER_BLOCKED.into(),
                            ..change(Waiting)
                        };
                        state.tasks.move_to(&id, block).await.unwrap();
                    }
                    ToolAfter => {}
                }
            }
            let task = state.tasks.get_for_test(&id).await;
            assert_eq!(task.state, to, "{name}");
            if summary.is_some() || to == Done {
                assert_eq!(task.summary.as_deref(), summary, "{name}");
            }
        }

        // While it works, the view says what it is doing: the running tool.
        let (state, id) = tracked(finished_agent(Vec::new())).await;
        let mut call = reply("m1", "");
        call.part = AgentPart::Tool(
            serde_json::from_value(json!({
                "id": "c1", "name": "bash", "title": "cargo test",
                "input": { "command": "cargo test" }, "state": "running", "status": "running"
            }))
            .unwrap(),
        );
        let upsert = AgentDomainEvent::TimelineUpsert {
            asid: AgentSessionId(ASID.into()),
            items: vec![call],
            seq: 1,
        };
        let mut updates = state.task_events.subscribe();
        let progress = || async {
            let task = state.tasks.get_for_test(&id).await;
            state.tasks.present(&task).await.progress
        };
        state.tasks.follow(&state, &upsert).await;
        assert_eq!(progress().await.as_deref(), Some("bash `cargo test`"));
        // The next line, within the throttle: kept at once, published once
        // the 2 s are up.
        let text = AgentDomainEvent::TimelineUpsert {
            asid: AgentSessionId(ASID.into()),
            items: vec![reply("m2", "All 12 tests pass.\nNext: docs")],
            seq: 2,
        };
        state.tasks.follow(&state, &text).await;
        assert_eq!(progress().await.as_deref(), Some("All 12 tests pass."));
        let published = |updates: &mut broadcast::Receiver<TaskDomainEvent>| {
            std::iter::from_fn(|| updates.try_recv().ok())
                .map(|TaskDomainEvent::Updated { task, .. }| task.progress)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            published(&mut updates),
            [Some("bash `cargo test`".to_owned())]
        );
        tokio::time::sleep(PROGRESS_EVERY + Duration::from_millis(300)).await;
        assert_eq!(
            published(&mut updates),
            [Some("All 12 tests pass.".to_owned())]
        );
        // A pause drops it: the hint says why it stopped.
        state
            .tasks
            .move_to(&id, change(TaskState::IdleUnconfirmed))
            .await
            .unwrap();
        assert_eq!(progress().await, None);

        // The title follows the session's own name -- not an untitled
        // session's placeholder (its asid, "New thread", DeepSeek's), or the
        // goal restated or cut short; a renamed session re-publishes its task.
        let title = || async {
            let task = state.tasks.get_for_test(&id).await;
            state.tasks.present(&task).await.title
        };
        for (name, expected) in [
            ("", "fix the build"),
            (ASID, "fix the build"),
            ("New thread", "fix the build"),
            ("DeepSeek Agent Session", "fix the build"),
            ("fix the…", "fix the build"),
            ("fix the build", "fix the build"),
            ("Fix the CI build", "Fix the CI build"),
        ] {
            let mut info = crate::test_support::fake_session(ASID, 1);
            info.title = name.into();
            let renamed = AgentDomainEvent::SessionUpdated {
                asid: AgentSessionId(ASID.into()),
                info: Box::new(info),
                seq: 3,
            };
            state.tasks.follow(&state, &renamed).await;
            assert_eq!(title().await, expected, "{name}");
        }
        let titles: Vec<String> = std::iter::from_fn(|| updates.try_recv().ok())
            .map(|TaskDomainEvent::Updated { task, .. }| task.title)
            .collect();
        assert!(
            titles.contains(&"Fix the CI build".to_owned()),
            "{titles:?}"
        );
        // An ended task keeps the name it had, stored, not held in memory.
        state
            .tasks
            .move_to(&id, change(TaskState::Done))
            .await
            .unwrap();
        let ended = state.tasks.get_for_test(&id).await;
        assert_eq!(ended.session_title.as_deref(), Some("Fix the CI build"));
        assert_eq!(state.tasks.live_title(&id), None);
        assert_eq!(title().await, "Fix the CI build");
    }

    #[test]
    fn a_folder_is_named_from_home() {
        // A home of its own, reached through a symlink as some are.
        let root = crate::test_state_dir().join(format!("home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("real/src/app")).unwrap();
        let home = root.join("home");
        std::os::unix::fs::symlink(root.join("real"), &home).unwrap();
        let under = |rest: &str| home.join(rest).to_str().unwrap().to_owned();
        for (dir, named) in [
            (under(""), "~"),
            (under("src/app"), "~/src/app"),
            ("/opt/elsewhere".to_owned(), "/opt/elsewhere"),
        ] {
            assert_eq!(home_relative(&dir, Some(&home)), named, "{dir}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_tool_call_reads_as_what_the_agent_is_doing() {
        let call = |name: &str, title: Option<&str>, input: Value| ToolCall {
            name: name.into(),
            title: title.map(str::to_owned),
            ..serde_json::from_value(json!({
                "id": "c1", "name": name, "input": input,
                "state": "running", "status": "running"
            }))
            .unwrap()
        };
        let patch = "*** Begin Patch\n*** Update File: release-notes/v3.1.0.md\n@@\n-a\n+b";
        for (tool, expected) in [
            (
                call(
                    "edit",
                    Some("Editing the notes"),
                    json!({ "filePath": "notes.md" }),
                ),
                Some("Editing the notes"),
            ),
            (
                call(
                    "read",
                    Some("README.md"),
                    json!({ "filePath": "docs/README.md" }),
                ),
                Some("read `docs/README.md`"),
            ),
            (
                call("patch", None, json!({ "patchText": patch })),
                Some("patch `release-notes/v3.1.0.md`"),
            ),
            (
                call(
                    "bash",
                    Some("cargo test"),
                    json!({ "command": "cargo test\ncargo fmt" }),
                ),
                Some("bash `cargo test`"),
            ),
            (
                call(
                    "bash",
                    None,
                    json!({ "command": "bun test src/lib/__tests__/cards.test.ts" }),
                ),
                Some("bash `bun test src/lib/__tests__/cards.test.ts`"),
            ),
            (
                call("glob", Some("a`b"), json!({ "pattern": "a`b" })),
                Some("glob ``a`b``"),
            ),
            // A long command is cut inside its fence, which stays closed.
            (
                call("bash", None, json!({ "command": "x".repeat(200) })),
                Some(&*format!("bash `{}…`", "x".repeat(110))),
            ),
            (call("todowrite", None, json!({ "todos": [] })), None),
            (call("execute", None, json!({})), None),
        ] {
            assert_eq!(tool_line(&tool).as_deref(), expected, "{}", tool.name);
        }
    }

    #[tokio::test]
    async fn an_idle_session_that_never_worked_is_not_done() {
        // `time_idle` is unset: the session has not been busy since.
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        assert!(
            !state
                .tasks
                .follow(&state, &status(AgentSessionStatus::Idle))
                .await
        );
        assert_eq!(
            state.tasks.get_for_test(&id).await.state,
            TaskState::Working
        );
    }

    #[tokio::test]
    async fn a_child_s_change_re_publishes_its_parent_with_fresh_counts() {
        let (state, parent) = delegated(FakeAgent::new("opencode")).await;
        let mut updates = state.task_events.subscribe();
        let by = Requested {
            forwarding: crate::tasks::domain::ForwardingContext::Unknown,
            by: assistant("agent:opencode"),
            origin: TaskOrigin::Agent,
            parent: Some(parent.clone()),
            name: None,
            server_id: None,
        };
        let mut second = request(None);
        second.label = Some("sub".into());
        let child = state.tasks.delegate(&state, by, second).await.unwrap();
        assert_eq!(child.parent_task_id.as_deref(), Some(parent.0.as_str()));
        assert_eq!(child.origin, TaskOrigin::Agent);
        assert_eq!(child.label.as_deref(), Some("sub"));
        let mut frames = Vec::new();
        while let Ok(TaskDomainEvent::Updated { task, .. }) = updates.try_recv() {
            frames.push(task);
        }
        // Queued, Dispatching, Working: each the child's frame, then the parent's.
        assert_eq!(frames.len(), 6);
        let parents: Vec<&TaskView> = frames.iter().filter(|f| f.task_id == parent.0).collect();
        assert_eq!(parents.len(), 3);
        assert_eq!(
            parents[2].children,
            ChildCounts {
                total: 1,
                active: 1,
                waiting: 0,
                failed: 0
            }
        );
        let listed = state.tasks.list_for(None, None, 10).await.unwrap();
        let listed_parent = listed.iter().find(|t| t.task_id == parent.0).unwrap();
        assert_eq!(listed_parent.children.total, 1);
        assert_eq!(listed_parent.parent_task_id, None);
        let tree = state.tasks.tree(&parent, None, 10).await.unwrap();
        assert_eq!(tree.len(), 2);
        assert_eq!(
            state
                .tasks
                .tree(&TaskId(child.task_id), None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// A failed or interrupted session ends its task; a failed one says why,
    /// in the agent's words or, without them, the timeline's.
    #[tokio::test]
    async fn a_failed_session_says_why_and_an_interrupted_one_is_cancelled() {
        use AgentSessionStatus::{Failed, Interrupted};
        use TaskState::*;
        let error = crate::agents::domain::AgentErrorInfo {
            name: "ProviderError".into(),
            message: "rate limited:\n try again later".into(),
            status: Some(429),
        };
        let rows = [
            (
                "the agent's error",
                Working,
                Failed,
                Some(error),
                "",
                Some("OpenCode's model provider refused the request (429)"),
            ),
            (
                "no error: what the session last said",
                Working,
                Failed,
                None,
                "Error: 401 Unauthorized",
                Some("the agent session failed: Error: 401 Unauthorized"),
            ),
            (
                "from a pause",
                IdleUnconfirmed,
                Failed,
                None,
                "",
                Some("the agent session failed"),
            ),
            ("interrupted", Working, Interrupted, None, "", None),
        ];
        for (name, from, status, error, said, summary) in rows {
            let timeline = if said.is_empty() {
                Vec::new()
            } else {
                vec![reply("m1", said)]
            };
            let agent = FakeAgent::new("opencode")
                .with_sessions(&[(ASID, 1)])
                .with_timeline(timeline);
            let (state, id) = tracked(agent).await;
            if from != Working {
                state.tasks.move_to(&id, change(from)).await.unwrap();
            }
            let event = AgentDomainEvent::StatusChanged {
                asid: AgentSessionId(ASID.into()),
                status,
                error,
                seq: 0,
            };
            assert!(state.tasks.follow(&state, &event).await, "{name}");
            let task = state.tasks.get_for_test(&id).await;
            let to = if status == Interrupted {
                Cancelled
            } else {
                TaskState::Failed
            };
            assert_eq!(task.state, to, "{name}");
            if summary.is_some() {
                assert_eq!(task.summary.as_deref(), summary, "{name}");
            }
        }
    }

    /// A turn's summary comes from the agent's own read or from the gateway's
    /// mirror of its stream, whichever holds this turn's newer answer.
    #[tokio::test]
    async fn the_summary_is_this_turns_newest_answer_from_the_page_or_the_stream() {
        let at = |mut item: TimelineItem, ms: u64| {
            item.updated_ms = ms;
            item
        };
        // Written after the task began.
        let now = now_ms() + 10_000;
        let rows = [
            (
                "the page lags the stream",
                vec![],
                at(reply("s8~a", "PONG\n"), now),
                "PONG",
            ),
            (
                "the page holds the last turn",
                vec![at(reply("m1", "an old answer"), 1)],
                at(reply("m2~a", "fresh"), now),
                "fresh",
            ),
            (
                "the page has this turn",
                vec![reply("m1", "from the agent")],
                at(reply("m0", "x"), now),
                "from the agent",
            ),
            (
                "the page has only commentary",
                vec![at(reply("m1", "Let me look."), now + 1_000)],
                at(reply("m2", "Fixed: green."), now + 2_000),
                "Fixed: green.",
            ),
        ];
        for (name, page, streamed, summary) in rows {
            let (state, id) = tracked(finished_agent(page)).await;
            let manager = state
                .agent_runtime
                .manager_for_agent("opencode")
                .await
                .unwrap();
            manager
                .mirror()
                .upsert_timeline_items(&AgentSessionId(ASID.into()), vec![streamed])
                .await;
            assert!(
                state
                    .tasks
                    .follow(&state, &status(AgentSessionStatus::Idle))
                    .await,
                "{name}"
            );
            let task = state.tasks.get_for_test(&id).await;
            assert_eq!(task.summary.as_deref(), Some(summary), "{name}");
            if name == "the page lags the stream" {
                let waited = state
                    .tasks
                    .wait(&state, &assistant("a"), &id, Duration::from_secs(1))
                    .await
                    .unwrap();
                assert_eq!(
                    waited.answer.as_deref(),
                    Some("PONG\n"),
                    "a waiter gets it whole"
                );
            }
        }
    }

    fn model_with_variants(provider_id: &str, id: &str, variants: &[&str]) -> ModelInfo {
        serde_json::from_value(json!({
            "id": id, "name": id, "provider_id": provider_id,
            "variants": variants.iter().map(|v| json!({ "id": v })).collect::<Vec<_>>()
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn the_variant_reaches_the_create_request_and_is_checked_against_the_catalog() {
        let state = test_state("admin", Vec::new());
        let mut agent = FakeAgent::new("opencode").with_sessions(&[(ASID, 1)]);
        agent.catalog.models = vec![
            model_with_variants("openai", "gpt-6", &["low", "high"]),
            model("anthropic", "claude-haiku-5-5"),
        ];
        let calls = agent.calls.clone();
        let created = agent.created_with.clone();
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let open = |model: Option<&str>, variant: Option<&str>| {
            let mut asked = request(None);
            asked.model = model.map(str::to_owned);
            asked.variant = variant.map(str::to_owned);
            asked
        };
        let delegate = |asked| {
            state
                .tasks
                .delegate(&state, Requested::mcp(assistant("a")), asked)
        };

        // A variant the model lists, a variant of a model that lists none
        // (passed through), and no variant at all.
        delegate(open(Some("openai/gpt-6"), Some("high")))
            .await
            .unwrap();
        delegate(open(Some("claude-haiku-5-5"), Some("max")))
            .await
            .unwrap();
        delegate(open(Some("gpt-6"), None)).await.unwrap();
        let variants: Vec<_> = created
            .lock()
            .unwrap()
            .iter()
            .map(|model| model.as_ref().and_then(|m| m.variant.clone()))
            .collect();
        assert_eq!(
            variants,
            [Some("high".to_owned()), Some("max".to_owned()), None]
        );

        // Not one of the model's variants: refused, and nothing more is
        // created. Without a model the variant is dropped.
        let before = calls.lock().unwrap().len();
        let unlisted = delegate(open(Some("gpt-6"), Some("max")))
            .await
            .unwrap_err();
        assert!(
            matches!(unlisted, TaskError::UnknownVariant { ref wanted, ref offered } if wanted == "max" && offered == "low, high")
        );
        assert_eq!(calls.lock().unwrap().len(), before);
        delegate(open(None, Some("high"))).await.unwrap();
        assert_eq!(created.lock().unwrap().last().unwrap(), &None);
    }
}
