//! The task use cases: delegate a goal to an agent session, wait for it,
//! read it, cancel it, and run commands through `run_command`.
//!
//! Every change the service records -- its own and `run_command`'s, which
//! write through [`Publishing`] -- is broadcast as a [`TaskDomainEvent`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
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
use super::ports::{TaskFuture, TaskStore, TaskStoreError};
use super::run_command::{self, RunHandle, RunRequest};
use crate::agents::domain::{
    AgentDomainEvent, AgentPart, AgentSessionId, AgentSessionInfo, AgentSessionStatus, ModelInfo,
    ModelRef, TimelineItem, TimelineRole,
};
use crate::agents::manager::AgentManager;
use crate::agents::ports::agent::AgentError;
use crate::agents::runtime::AGENT_PREFERENCE;
use crate::fleet::manifest::is_self;
use crate::AppState;

/// The longest a `wait` blocks, and the shortest.
const MAX_WAIT: Duration = Duration::from_secs(45);
const MIN_WAIT: Duration = Duration::from_secs(1);
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

pub(crate) struct TaskService {
    store: Arc<dyn TaskStore>,
    events: broadcast::Sender<TaskDomainEvent>,
    machine_id: String,
}

pub(crate) struct DelegateRequest {
    pub goal: String,
    pub cwd: Option<String>,
    pub agent_id: Option<String>,
    pub mode: Option<String>,
    pub model: Option<String>,
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
    /// No configured terminal session answers, so a command has nowhere to run.
    NoTerminal,
    /// A timeline was asked of a task that has none (a command).
    NoTimeline,
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
            Self::NoTerminal => write!(f, "{}", run_command::NO_TERMINAL),
            Self::NoTimeline => write!(f, "only a delegated task has a timeline"),
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
    /// Idle: done, if the agent went busy first.
    Idle,
    Move(TaskChange),
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
            AgentSessionStatus::Failed => {
                let line = error.as_ref().map(|e| format!("{}: {}", e.name, e.message));
                Step::Move(TaskChange {
                    summary: Some(one_line(
                        line.as_deref().unwrap_or("the agent session failed"),
                        SUMMARY_CHARS,
                    )),
                    note: line,
                    ..change(TaskState::Failed)
                })
            }
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
        }
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
        let manager = agent_for(state, request.agent_id.as_deref()).await?;
        let agent_id = manager.agent().kind();
        let model = match request.model.as_deref() {
            Some(id) => {
                let catalog = manager
                    .sessions()
                    .get_catalog(request.cwd.as_deref())
                    .await?;
                Some(
                    resolve_model(&catalog.models, id)
                        .ok_or_else(|| TaskError::UnknownModel(id.to_owned()))?,
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
            manager
                .prompts()
                .send_prompt(&asid, &request.goal, &[], None)
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
    /// not settle it): done now, not at the driver's next pass.
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
    /// or cancelled, or to waiting on the user. A command's answer carries
    /// what it printed so far.
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
            TaskBinding::AgentSession { asid, .. } if view.state == TaskState::Done => {
                full_answer(state, asid).await
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
                Step::Idle => {
                    let items = match manager.agent().get_timeline(asid, SUMMARY_ITEMS).await {
                        Ok(items) => items,
                        Err(error) => {
                            tracing::warn!(
                                "task {}: no summary, timeline read failed: {error}",
                                task.id.0
                            );
                            Vec::new()
                        }
                    };
                    TaskChange {
                        summary: result_line(&items),
                        ..change(TaskState::Done)
                    }
                }
                Step::Move(to) => to,
            };
            if to.to == task.state {
                // Still waiting on the user: say so now, there is nothing to
                // wait for until they answer.
                if task.state == TaskState::Waiting {
                    return Ok(self.present(&task).await);
                }
                continue;
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
            if task.state.is_terminal() || target == TaskState::Waiting {
                return Ok(self.present(&task).await);
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
        Self::view(task, counts)
    }

    /// [`Self::present`] for a page of tasks, counted in one query.
    async fn present_all(&self, tasks: &[Task]) -> anyhow::Result<Vec<TaskView>> {
        let ids = tasks.iter().map(|task| task.id.clone()).collect();
        let counts = self.store.child_counts(ids).await?;
        Ok(tasks
            .iter()
            .map(|task| Self::view(task, counts.get(&task.id).copied().unwrap_or_default()))
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
        }
    }

    /// Settle the open task bound to the session an agent event is about, as
    /// it happens, so the ledger moves without anyone waiting: a finished
    /// turn is done with its result line, a failed session failed with its
    /// error line, an interrupted one cancelled, a pending request waiting.
    /// Whether a task moved.
    pub(crate) async fn follow(&self, state: &AppState, event: &AgentDomainEvent) -> bool {
        let asid = match event {
            AgentDomainEvent::StatusChanged {
                asid,
                status:
                    AgentSessionStatus::Idle
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
        let open = self
            .store
            .list(TaskFilter {
                asid: Some(asid.0.clone()),
                states: Some(OPEN_STATES.to_vec()),
                limit: 1,
                ..Default::default()
            })
            .await;
        let task = match open {
            Ok(mut tasks) if !tasks.is_empty() => tasks.remove(0),
            Ok(_) => return false,
            Err(error) => {
                tracing::debug!(%error, "task follower: lookup failed");
                return false;
            }
        };
        let to = match step(event, &asid.0) {
            Step::Idle => match self.finished_change(state, &task).await {
                Some(done) => done,
                None => return false,
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

    /// Done with its result line, if `task`'s session went idle after the
    /// task was created (it worked on the prompt in between).
    async fn finished_change(&self, state: &AppState, task: &Task) -> Option<TaskChange> {
        let TaskBinding::AgentSession { agent_id, asid } = &task.binding else {
            return None;
        };
        let manager = state.agent_runtime.manager_for_agent(agent_id).await?;
        let session = manager.sessions().get_session(asid).await.ok()?;
        if !finished_since(&session, task.created_ms) {
            return None;
        }
        let summary = match manager.agent().get_timeline(asid, SUMMARY_ITEMS).await {
            Ok(items) => result_line(&items),
            Err(error) => {
                tracing::warn!(task = %task.id.0, %error, "task follower: no summary");
                None
            }
        };
        Some(TaskChange {
            summary,
            ..change(TaskState::Done)
        })
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

/// A delegation's summary: the last assistant message as one line.
pub(crate) fn result_line(items: &[TimelineItem]) -> Option<String> {
    last_assistant_text(items).map(|text| one_line(&text, SUMMARY_CHARS))
}

/// The last assistant message of `asid`, in full; `None` when it cannot be read.
async fn full_answer(state: &AppState, asid: &str) -> Option<String> {
    let manager = state.agent_runtime.manager_for_session(asid).await?;
    match manager.agent().get_timeline(asid, SUMMARY_ITEMS).await {
        Ok(items) => last_assistant_text(&items),
        Err(error) => {
            tracing::debug!(%error, "task wait: no answer");
            None
        }
    }
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
    let running = matches!(
        session.status,
        AgentSessionStatus::Busy | AgentSessionStatus::Retry
    );
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
fn resolve_model(models: &[ModelInfo], id: &str) -> Option<ModelRef> {
    let qualified = id.split_once('/').and_then(|(provider, model_id)| {
        models
            .iter()
            .find(|model| model.provider_id == provider && model.id == model_id)
    });
    qualified
        .or_else(|| models.iter().find(|model| model.id == id))
        .map(|found| ModelRef {
            provider_id: found.provider_id.clone(),
            model_id: found.id.clone(),
            variant: None,
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
        let calls = agent.calls.clone();
        let (state, id) = delegated(agent).await;
        assert_eq!(*calls.lock().unwrap(), ["create_session", "send_prompt"]);
        let view = state
            .tasks
            .list_for(None, None, 10)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(view.task_id, id.0);
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
        let bare = resolve_model(&models, "claude-haiku-5-5").unwrap();
        assert_eq!(
            (bare.provider_id.as_str(), bare.model_id.as_str()),
            ("openrouter", "claude-haiku-5-5")
        );
        let pinned = resolve_model(&models, "anthropic/claude-haiku-5-5").unwrap();
        assert_eq!(
            (pinned.provider_id.as_str(), pinned.model_id.as_str()),
            ("anthropic", "claude-haiku-5-5")
        );
        // An id that itself carries a slash still matches as a bare id.
        let slashed = resolve_model(&models, "anthropic/claude-opus").unwrap();
        assert_eq!(slashed.provider_id, "openrouter");
        assert_eq!(resolve_model(&models, "gpt-9"), None);
        assert_eq!(resolve_model(&models, "anthropic/gpt-9"), None);
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
        assert_eq!(done.state, TaskState::Done);
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
        assert_eq!(done.state, TaskState::Done);
        assert_eq!(done.summary.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn wait_returns_waiting_on_a_pending_permission() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let pending = vec![permission("someone_else"), permission(ASID)];
        let waited = wait_through(&state, &id, pending).await;
        assert_eq!(waited.state, TaskState::Waiting);

        let finished = vec![
            resolved(),
            status(AgentSessionStatus::Busy),
            status(AgentSessionStatus::Idle),
        ];
        let done = wait_through(&state, &id, finished).await;
        assert_eq!(done.state, TaskState::Done);
    }

    /// A request raised between two waits is seen by the next one.
    #[tokio::test]
    async fn a_permission_pending_before_the_wait_is_waiting() {
        let agent = FakeAgent::new("opencode");
        let pending = agent.permissions.clone();
        pending.lock().unwrap().push(permission_request(ASID));
        let (state, id) = delegated(agent).await;
        let waited = state
            .tasks
            .wait(&state, &assistant("a"), &id, Duration::from_secs(5))
            .await
            .map(|waited| waited.task)
            .unwrap();
        assert_eq!(waited.state, TaskState::Waiting);

        // Answered on the phone: nothing pending any more.
        pending.lock().unwrap().clear();
        let finished = vec![
            resolved(),
            status(AgentSessionStatus::Busy),
            status(AgentSessionStatus::Idle),
        ];
        let done = wait_through(&state, &id, finished).await;
        assert_eq!(done.state, TaskState::Done);
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
    async fn a_failed_or_interrupted_session_ends_the_task() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let failed = wait_through(&state, &id, vec![status(AgentSessionStatus::Failed)]).await;
        assert_eq!(failed.state, TaskState::Failed);

        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let interrupted = vec![status(AgentSessionStatus::Interrupted)];
        let stopped = wait_through(&state, &id, interrupted).await;
        assert_eq!(stopped.state, TaskState::Cancelled);
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
    async fn a_finished_turn_settles_the_task_with_its_result_line_as_it_happens() {
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
        assert_eq!(task.state, TaskState::Done);
        let summary = task.summary.unwrap();
        assert!(
            summary.starts_with("Fixed it. The build was very"),
            "{summary}"
        );
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= SUMMARY_CHARS && summary.ends_with('…'));
        assert_eq!(task.exit_code, None);
        let TaskDomainEvent::Updated { task: frame, .. } = updates.try_recv().unwrap();
        assert_eq!(frame.state, TaskState::Done);
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
    async fn a_turn_that_ends_during_dispatch_is_done_when_delegate_returns() {
        let state = test_state("admin", Vec::new());
        let agent = finished_agent(vec![reply("m1", "quick")]);
        state.agent_runtime.attach_for_test(agent.manager()).await;
        let view = state
            .tasks
            .delegate(&state, Requested::mcp(assistant("a")), request(None))
            .await
            .unwrap();
        assert_eq!(view.state, TaskState::Done);
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

    /// An unconfirmed idle task can only leave for Working, Done or
    /// Cancelled; a failure passes through Working.
    #[tokio::test]
    async fn an_unconfirmed_idle_task_still_fails() {
        let (state, id) = tracked(FakeAgent::new("opencode").with_sessions(&[(ASID, 1)])).await;
        state
            .tasks
            .move_to(&id, change(TaskState::IdleUnconfirmed))
            .await
            .unwrap();
        let failed = AgentDomainEvent::StatusChanged {
            asid: AgentSessionId(ASID.into()),
            status: AgentSessionStatus::Failed,
            error: None,
            seq: 0,
        };
        assert!(state.tasks.follow(&state, &failed).await);
        let task = state.tasks.get_for_test(&id).await;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(task.summary.as_deref(), Some("the agent session failed"));
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
    async fn a_failed_session_fails_its_task_with_the_error_line() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        let failed = AgentDomainEvent::StatusChanged {
            asid: AgentSessionId(ASID.into()),
            status: AgentSessionStatus::Failed,
            error: Some(crate::agents::domain::AgentErrorInfo {
                name: "ProviderError".into(),
                message: "rate limited:\n try again later".into(),
                status: Some(429),
            }),
            seq: 0,
        };
        assert!(state.tasks.follow(&state, &failed).await);
        let task = state.tasks.get_for_test(&id).await;
        assert_eq!(task.state, TaskState::Failed);
        assert_eq!(
            task.summary.as_deref(),
            Some("ProviderError: rate limited: try again later")
        );
    }

    #[tokio::test]
    async fn a_permission_request_makes_the_task_wait_and_its_answer_resumes_it() {
        let (state, id) = delegated(FakeAgent::new("opencode")).await;
        assert!(state.tasks.follow(&state, &permission(ASID)).await);
        assert_eq!(
            state.tasks.get_for_test(&id).await.state,
            TaskState::Waiting
        );
        assert!(state.tasks.follow(&state, &resolved()).await);
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
}
