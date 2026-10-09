//! The task use cases: delegate a goal to an agent session, wait for it,
//! read it, cancel it, and run commands through `run_command`.
//!
//! Every change the service records -- its own and `run_command`'s, which
//! write through [`Publishing`] -- is broadcast as a [`TaskDomainEvent`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use super::domain::{
    clip, now_ms, one_line, ChildCounts, PrincipalKind, Requested, Task, TaskBinding, TaskChange,
    TaskEvent, TaskFilter, TaskId, TaskKind, TaskOrigin, TaskPrincipal, TaskState,
};
use super::events::TaskDomainEvent;
use super::pane_agent;
use super::ports::{TaskFuture, TaskStore, TaskStoreError};
use super::run_command::{self, RunHandle, RunRequest};
use crate::agents::domain::{
    AgentDomainEvent, AgentErrorInfo, AgentPart, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, ModelInfo, ModelRef, TimelineItem, TimelineRole,
};
use crate::agents::manager::AgentManager;
use crate::agents::ports::agent::AgentError;
use crate::agents::ports::mirror::SessionMirrorPort;
use crate::agents::runtime::AGENT_PREFERENCE;
use crate::backend::{PaneId, TabId};
use crate::fleet::manifest::is_self;
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
}

/// Who asked for a task, as a task view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RequestedBy {
    pub kind: PrincipalKind,
    pub id: String,
    pub name: String,
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
    pub updated_ms: u64,
    /// The open task this one was started from, if any.
    pub parent_task_id: Option<String>,
    pub origin: TaskOrigin,
    pub label: Option<String>,
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
    UnknownMachine(String),
    AgentUnavailable(String),
    /// `model` names nothing in the agent's catalog for that directory.
    UnknownModel(String),
    /// `variant` is not one the resolved model lists.
    UnknownVariant(String),
    /// No configured terminal session answers, so a command has nowhere to run.
    NoTerminal,
    /// A timeline was asked of a task that has none (a command).
    NoTimeline,
    /// The device confirmed a task that is not its own paused or waiting
    /// delegation.
    NotConfirmable,
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
            Self::NotFound => write!(f, "task not found"),
            Self::NotYours => write!(f, "task belongs to someone else"),
            Self::UnknownMachine(machine) => write!(f, "unknown machine '{machine}'"),
            Self::AgentUnavailable(agent) => write!(f, "agent '{agent}' is not available"),
            Self::UnknownModel(model) => write!(f, "unknown model '{model}'"),
            Self::UnknownVariant(variant) => write!(f, "unknown variant {variant}"),
            Self::NoTerminal => write!(f, "{}", run_command::NO_TERMINAL),
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

    fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task> {
        Box::pin(async move {
            let task = self.0.store.transition(id, change).await?;
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
}

fn change(to: TaskState) -> TaskChange {
    TaskChange {
        to,
        source: SOURCE.into(),
        ..Default::default()
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
    Move(TaskChange),
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
            AgentSessionStatus::Interrupted => Step::Move(change(TaskState::Cancelled)),
            AgentSessionStatus::Unknown => Step::Stay,
        },
        AgentDomainEvent::PermissionPending { asid: id, .. }
        | AgentDomainEvent::FormPending { asid: id, .. }
            if ours(id) =>
        {
            Step::Move(change(TaskState::Waiting))
        }
        AgentDomainEvent::PermissionResolved { asid: id, .. }
        | AgentDomainEvent::FormResolved { asid: id, .. }
            if ours(id) =>
        {
            Step::Move(change(TaskState::Working))
        }
        _ => Step::Stay,
    }
}

impl TaskService {
    pub(crate) fn new(
        store: Arc<dyn TaskStore>,
        events: broadcast::Sender<TaskDomainEvent>,
        machine_id: String,
    ) -> Self {
        Self {
            store,
            events,
            machine_id,
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
        if task.state.is_terminal() || paused(task.state) {
            return None;
        }
        let live = self.live.lock().ok()?;
        live.lines.get(&task.id).map(|entry| entry.line.clone())
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
        }
    }

    /// A session's timeline changed: the open task bound to it is doing
    /// what its newest item says. The task is looked up once per session
    /// (a session bound to none is asked again after [`UNBOUND_RECHECK`]).
    async fn follow_progress(&self, state: &AppState, asid: &str, items: &[TimelineItem]) {
        let known = match self.live.lock() {
            Ok(live) => live.sessions.get(asid).cloned(),
            Err(_) => return,
        };
        let task = match known {
            Some((Some(id), _)) => id,
            Some((None, at)) if at.elapsed() < UNBOUND_RECHECK => return,
            _ => {
                let found = self.open_task_for(asid).await.map(|task| task.id);
                if let Ok(mut live) = self.live.lock() {
                    live.sessions
                        .insert(asid.to_owned(), (found.clone(), Instant::now()));
                }
                match found {
                    Some(id) => id,
                    None => return,
                }
            }
        };
        if let Some(line) = items.iter().rev().find_map(progress_line) {
            self.note_progress(state, &task, &line).await;
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
        if !is_self(request.machine.as_deref(), &self.machine_id) {
            return Err(TaskError::UnknownMachine(
                request.machine.unwrap_or_default(),
            ));
        }
        if let Some(kind) = request.agent_id.as_deref() {
            // A session agent (OpenCode, DeepSeek, T3) is never started in a
            // pane: unattached, it is unavailable.
            if !AGENT_PREFERENCE.contains(&kind)
                && state.agent_runtime.manager_for_agent(kind).await.is_none()
                && pane_agent::is_terminal_agent(state, kind)
            {
                return self.delegate_to_pane(state, by, request).await;
            }
        }
        let manager = agent_for(state, request.agent_id.as_deref()).await?;
        let agent_id = manager.agent().kind();
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
                        .map_err(TaskError::UnknownVariant)?,
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
        let dispatched = async {
            let dispatching = TaskChange {
                binding: Some(binding.clone()),
                ..change(TaskState::Dispatching)
            };
            self.store().transition(&task.id, dispatching).await?;
            self.bind_session(&asid.0, &task.id);
            // Asked for an explicit end: a turn that stops is only a pause.
            let prompt = pane_agent::completion_request(&request.goal);
            manager
                .prompts()
                .send_prompt(&asid, &prompt, &[], None)
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
        let session = run_command::reachable_session(state)
            .await
            .ok_or(TaskError::NoTerminal)?
            .0;
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
        let title = pane_agent::pane_title(request.label.as_deref(), &request.goal);
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
        let cwd = request.cwd.clone();
        let prompt = pane_agent::completion_request(&request.goal);
        tokio::spawn(async move {
            let start = tokio::spawn(start_terminal_agent(
                state.clone(),
                session,
                kind,
                cwd,
                placed,
                prompt,
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

/// The background half of [`TaskService::delegate_to_pane`]: start the agent
/// in its placed pane with the goal (on its command line where it takes one,
/// otherwise typed) unless the task ended meanwhile -- a cancel, or a
/// follower that gave up on it -- then move the task to working and follow
/// it -- or fail it naming the step.
pub(crate) async fn start_terminal_agent(
    state: AppState,
    session: crate::SessionConfig,
    kind: String,
    cwd: Option<String>,
    placed: crate::agents::session_routes::PlacedAgent,
    prompt: String,
    task: Task,
) {
    use crate::agents::session_routes::{start_agent_in_place, type_agent_prompt};
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
    let (mut launched, to_type) = start_agent_in_place(
        &state,
        &session,
        &kind,
        cwd.as_deref(),
        placed,
        Some(&prompt),
    )
    .await;
    let typed = to_type.is_some();
    if let Some(started) = to_type {
        if ended().await {
            tracing::debug!(task = %task.id.0, "terminal task ended while its agent started: goal not typed");
            return;
        }
        type_agent_prompt(&session, &started, Some(&prompt), &mut launched).await;
    }
    let why = match failed_step(&launched.steps.value()) {
        Some(why) => Some(why),
        // Typed is not sent: Claude Code can keep a pasted goal in its box.
        None if typed && !pane_agent::goal_accepted(&session, &pane_id, &prompt).await => {
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
    let to = change(TaskState::Working);
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

    /// [`Self::move_to`], logging instead of failing: the caller is already
    /// reporting an error. The failure's note is its summary, unless it has one.
    async fn record_failure(&self, task: &Task, mut failed: TaskChange) {
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
            _ if task.state.is_terminal() => self.present(&task).await,
            TaskBinding::Pane { .. } if task.kind == TaskKind::Delegation => {
                self.wait_published(&task.id, deadline).await?
            }
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
        let since = task.created_ms;
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
                Step::Idle => turn_end(&summary_timeline(&manager, asid, task.created_ms).await),
                Step::Failed(error) => failed_change(&manager, &task, error.as_ref()).await,
                Step::Move(to) => to,
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
            let target = to.to;
            task = match self.settle(&task, to).await {
                Ok(task) => task,
                // Moved meanwhile (another waiter, a cancel): that is the answer.
                Err(error) if is_illegal_transition(&error) => {
                    let now = self.store.get(&task.id).await?.ok_or(TaskError::NotFound)?;
                    return Ok(self.present(&now).await);
                }
                Err(error) => return Err(error),
            };
            if task.state.is_terminal() || paused(target) {
                return Ok(self.present(&task).await);
            }
        }
    }

    /// Wait for the task to be recorded as finished, waiting on the user or
    /// paused (by whoever moves it), or for `deadline`; the task as it then
    /// stands.
    async fn wait_published(&self, id: &TaskId, deadline: Instant) -> Result<TaskView, TaskError> {
        let settled = |task: &Task| task.state.is_terminal() || paused(task.state);
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
    /// one that fails).
    async fn settle(&self, task: &Task, to: TaskChange) -> Result<Task, TaskError> {
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

    /// Interrupt the agent (or Ctrl-C the command) and mark the task Cancelled.
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
            TaskBinding::Pane { .. } => {
                run_command::cancel(state, &self.store(), &task)
                    .await
                    .map_err(|e| TaskError::Backend(format!("{e:#}")))?;
                let task = self.store.get(&task.id).await?.ok_or(TaskError::NotFound)?;
                return Ok(self.present(&task).await);
            }
            TaskBinding::AgentSession { agent_id, asid } => {
                session_manager(state, agent_id, asid)
                    .await?
                    .prompts()
                    .interrupt(&AgentSessionId(asid.clone()))
                    .await?;
            }
            TaskBinding::None => {}
        }
        let task = self
            .store()
            .transition(&task.id, change(TaskState::Cancelled))
            .await?;
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
        run_command::start(state, &self.store(), by, request)
            .await
            .map_err(command_error)
    }

    /// Open tasks for the driver, as stored.
    pub(crate) async fn open_tasks(&self, filter: TaskFilter) -> anyhow::Result<Vec<Task>> {
        self.store.list(filter).await
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

    /// `task` as clients see it, with its children counted.
    pub(crate) async fn present(&self, task: &Task) -> TaskView {
        let counts = match self.store.child_counts(vec![task.id.clone()]).await {
            Ok(mut counts) => counts.remove(&task.id).unwrap_or_default(),
            Err(error) => {
                tracing::warn!(task = %task.id.0, %error, "task children not counted");
                ChildCounts::default()
            }
        };
        TaskView {
            progress: self.progress_of(task),
            ..Self::view(task, counts)
        }
    }

    /// [`Self::present`] for a page of tasks, counted in one query.
    async fn present_all(&self, tasks: &[Task]) -> anyhow::Result<Vec<TaskView>> {
        let ids = tasks.iter().map(|task| task.id.clone()).collect();
        let counts = self.store.child_counts(ids).await?;
        Ok(tasks
            .iter()
            .map(|task| TaskView {
                progress: self.progress_of(task),
                ..Self::view(task, counts.get(&task.id).copied().unwrap_or_default())
            })
            .collect())
    }

    pub(crate) fn view(task: &Task, children: ChildCounts) -> TaskView {
        let requested_by = match (&task.origin, &task.requester_name) {
            (TaskOrigin::Device, Some(name)) => Some(RequestedBy {
                kind: task.created_by.kind,
                id: task.created_by.id.clone(),
                name: name.clone(),
            }),
            _ => None,
        };
        TaskView {
            task_id: task.id.0.clone(),
            state: task.state,
            kind: task.kind,
            goal: task.goal.clone(),
            summary: task.summary.clone(),
            exit_code: task.exit_code,
            binding: task.binding.clone(),
            updated_ms: task.updated_ms,
            parent_task_id: task.parent.as_ref().map(|id| id.0.clone()),
            origin: task.origin,
            label: task.label.clone(),
            requested_by,
            children,
            progress: None,
        }
    }

    /// Settle the open task bound to the session an agent event is about, as
    /// it happens, so the ledger moves without anyone waiting: a finished
    /// turn is done on its completion marker and `idle_unconfirmed` without
    /// one (see [`turn_end`]), a paused task whose session runs again
    /// working, a failed session failed with its error line, an interrupted
    /// one cancelled, a pending request waiting. Whether a task moved.
    pub(crate) async fn follow(&self, state: &AppState, event: &AgentDomainEvent) -> bool {
        if let AgentDomainEvent::TimelineUpsert { asid, items, .. } = event {
            self.follow_progress(state, &asid.0, items).await;
            return false;
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
            Step::Idle => match self.finished_change(state, &task).await {
                Some(done) => done,
                None => return false,
            },
            // A new turn after a pause: the user (or the agent) went on.
            Step::Busy if task.state == TaskState::IdleUnconfirmed => change(TaskState::Working),
            Step::Failed(error) => match state.agent_runtime.manager_for_session(&asid.0).await {
                Some(manager) => failed_change(&manager, &task, error.as_ref()).await,
                None => failed(
                    session_failure(None, error.as_ref(), &asid.0, 0).await,
                    error,
                ),
            },
            Step::Move(to) if to.to != task.state => to,
            _ => return false,
        };
        let to = TaskChange {
            source: FOLLOWER.into(),
            ..to
        };
        match self.settle(&task, to).await {
            Ok(_) => true,
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
        if !finished_since(&session, task.created_ms) {
            return None;
        }
        let to = turn_end(&summary_timeline(&manager, asid, task.created_ms).await);
        (to.to != task.state).then_some(to)
    }

    /// The agent working on `id` says it is finished (`tasks.complete`; the
    /// caller has checked the agent owns the task): done, with `summary`.
    /// A task that has already ended is answered as it stands.
    pub(crate) async fn complete(&self, id: &TaskId, summary: &str) -> Result<TaskView, TaskError> {
        let done = TaskChange {
            summary: Some(summary_line(summary)?.ok_or(TaskError::InvalidSummary)?),
            source: "tasks_complete".into(),
            ..change(TaskState::Done)
        };
        self.finish(id, done).await
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
        if task.state == TaskState::Done && task.origin == TaskOrigin::Device {
            return Ok(self.present(&task).await);
        }
        let confirmable = task.kind == TaskKind::Delegation
            && task.origin == TaskOrigin::Device
            && paused(task.state);
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

    /// `to` (done) through the states between; a task that ended meanwhile
    /// is answered as it stands.
    async fn finish(&self, id: &TaskId, to: TaskChange) -> Result<TaskView, TaskError> {
        match self.move_to(id, to).await {
            Ok(task) => Ok(self.present(&task).await),
            Err(error) if is_illegal_transition(&error) => {
                let now = self.store.get(id).await?.ok_or(TaskError::NotFound)?;
                if now.state.is_terminal() {
                    return Ok(self.present(&now).await);
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }
}

/// What a timeline item says an agent is doing, as a progress line: a tool
/// call (its title, else its name and command or file; a failed call with
/// its error), or the start of an assistant message. Nothing for the rest.
fn progress_line(item: &TimelineItem) -> Option<String> {
    match &item.part {
        AgentPart::Tool(call) => {
            let what = call.title.clone().unwrap_or_else(|| {
                let target = ["command", "filePath", "file_path", "path", "pattern", "url"]
                    .iter()
                    .find_map(|key| call.input.get(*key).and_then(Value::as_str));
                match target {
                    Some(target) => format!("{} {target}", call.name),
                    None => call.name.clone(),
                }
            });
            Some(match &call.error {
                Some(error) => format!("{what} failed: {}: {}", error.name, error.message),
                None => what,
            })
        }
        AgentPart::Text { text } if item.role == TimelineRole::Assistant => text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned),
        _ => None,
    }
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
/// screen is read by) is `done`, with the marker's summary. Without it the
/// task is `idle_unconfirmed`, its summary the last message's line as a hint.
pub(crate) fn turn_end(items: &[TimelineItem]) -> TaskChange {
    let last = last_assistant_text(items);
    match last.as_deref().and_then(pane_agent::done_marker) {
        Some(summary) => TaskChange {
            summary: Some(summary),
            ..change(TaskState::Done)
        },
        None => TaskChange {
            summary: last.map(|text| one_line(&text, SUMMARY_CHARS)),
            ..change(TaskState::IdleUnconfirmed)
        },
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
        return one_line(&format!("{}: {}", error.name, error.message), SUMMARY_CHARS);
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

/// The timeline a finished delegation's summary is read from.
///
/// The agent's own read is asked first (OpenCode persists its answer before
/// it goes idle). It can lag the stream, though: DeepSeek reports the turn's
/// end from its live stream while `session/page` has yet to commit the final
/// assistant message, and then holds no answer for this turn at all. The
/// gateway's mirror of the session has the streamed rows by then, so when the
/// agent's read lacks an assistant answer written since `since_ms` (the
/// task's start), the mirror is read instead.
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
async fn full_answer(state: &AppState, asid: &str, since_ms: u64) -> Option<String> {
    let manager = state.agent_runtime.manager_for_session(asid).await?;
    last_assistant_text(&summary_timeline(&manager, asid, since_ms).await)
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
) -> Option<Result<ModelRef, String>> {
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
                    return Err(format!(
                        "'{wanted}' (the model offers: {})",
                        offered.join(", ")
                    ));
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
    async fn delegate_creates_a_session_and_a_working_task() {
        let agent = FakeAgent::new("opencode");
        let (calls, prompts) = (agent.calls.clone(), agent.prompts.clone());
        let (state, id) = delegated(agent).await;
        assert_eq!(*calls.lock().unwrap(), ["create_session", "send_prompt"]);
        // Asked for an explicit end; the goal is stored as given.
        let sent = [pane_agent::completion_request("fix the build")];
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
            matches!(&refused, Err(TaskError::UnknownMachine(m)) if m == "mac"),
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
        assert_eq!(steps_between(Waiting, Failed), [Working]);
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
            Complete(&'static str),
        }
        use Input::*;
        use TaskState::*;
        let idle = || Event(Box::new(status(AgentSessionStatus::Idle)));
        let marked = "Done.\n\nMUQUN_TASK_DONE: the build is green";
        let rows = [
            (
                "a turn that stops",
                Working,
                "Shall I update the docs too?",
                idle(),
                IdleUnconfirmed,
                Some("Shall I update the docs too?"),
            ),
            (
                "the marker",
                Working,
                marked,
                idle(),
                Done,
                Some("the build is green"),
            ),
            (
                "the marker's placeholder",
                Working,
                "MUQUN_TASK_DONE: <one-line summary>",
                idle(),
                IdleUnconfirmed,
                None,
            ),
            (
                "a question the gateway sees",
                Working,
                "",
                Event(Box::new(permission(ASID))),
                Waiting,
                None,
            ),
            (
                "a new turn after a pause",
                IdleUnconfirmed,
                "",
                Event(Box::new(status(AgentSessionStatus::Busy))),
                Working,
                None,
            ),
            (
                "the marker after a pause",
                IdleUnconfirmed,
                marked,
                idle(),
                Done,
                Some("the build is green"),
            ),
            (
                "tasks.complete",
                Working,
                "",
                Complete("tests pass"),
                Done,
                Some("tests pass"),
            ),
            (
                "a question answered",
                Waiting,
                "",
                Event(Box::new(resolved())),
                Working,
                None,
            ),
            (
                "tasks.complete while waiting",
                Waiting,
                "",
                Complete("tests pass"),
                Done,
                Some("tests pass"),
            ),
        ];
        for (name, from, last, input, to, summary) in rows {
            let timeline = if last.is_empty() {
                Vec::new()
            } else {
                vec![reply("m1", last)]
            };
            let (state, id) = tracked(finished_agent(timeline)).await;
            if from != Working {
                state.tasks.move_to(&id, change(from)).await.unwrap();
            }
            match input {
                Event(event) => assert!(state.tasks.follow(&state, &event).await, "{name}"),
                Complete(text) => {
                    state.tasks.complete(&id, text).await.unwrap();
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
        assert_eq!(progress().await.as_deref(), Some("cargo test"));
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
        assert_eq!(published(&mut updates), [Some("cargo test".to_owned())]);
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
            by: assistant("agent:opencode"),
            origin: TaskOrigin::Agent,
            parent: Some(parent.clone()),
            name: None,
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
                Some("ProviderError: rate limited: try again later"),
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
            matches!(unlisted, TaskError::UnknownVariant(ref v) if v.contains("'max'") && v.contains("low, high"))
        );
        assert_eq!(calls.lock().unwrap().len(), before);
        delegate(open(None, Some("high"))).await.unwrap();
        assert_eq!(created.lock().unwrap().last().unwrap(), &None);
    }
}
