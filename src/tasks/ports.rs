use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::domain::{
    ChildCounts, NoticeTarget, Task, TaskChange, TaskEvent, TaskFilter, TaskId, TaskPrincipal,
    TaskState,
};

pub(crate) type TaskFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

pub(crate) trait TaskStore: Send + Sync {
    /// Assigns `seq` and appends the creation event.
    fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task>;
    fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>>;
    /// The cwd of the most recently created device task, if it has one.
    fn latest_device_cwd<'a>(&'a self, device: &'a TaskPrincipal)
        -> TaskFuture<'a, Option<String>>;
    /// Newest updated first.
    fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>>;
    /// Refuses illegal moves with `TaskStoreError::IllegalTransition`.
    fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task>;
    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>>;
    /// Each of `ids`' direct children, counted by state; a task with none is
    /// absent from the map.
    fn child_counts<'a>(&'a self, ids: Vec<TaskId>)
        -> TaskFuture<'a, HashMap<TaskId, ChildCounts>>;
    /// Delete the tasks that ended before `before_ms` (none of whose
    /// children is still open), with their events. How many went.
    fn purge_ended<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize>;
    /// Where task `id`'s end is told, if anywhere.
    fn notice_of<'a>(&'a self, _id: &'a TaskId) -> TaskFuture<'a, Option<NoticeTarget>> {
        Box::pin(async { Ok(None) })
    }
    /// Tell `target` how task `id` ends (see `tasks::notices`).
    fn set_notice<'a>(&'a self, _id: &'a TaskId, _target: &'a NoticeTarget) -> TaskFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    /// Tasks with a notice target, updated since `since_ms`, in a reported
    /// state that has not been told yet: newest change first, at most `limit`.
    fn pending_notices<'a>(
        &'a self,
        _since_ms: u64,
        _limit: usize,
    ) -> TaskFuture<'a, Vec<(Task, NoticeTarget)>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    /// `task` as it is now (its state, at its `seq`) has been told: a later
    /// move into a reported state, even the same one again, is told anew.
    fn mark_told<'a>(&'a self, _task: &'a Task, _at_ms: u64) -> TaskFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
pub(crate) enum TaskStoreError {
    NotFound,
    IllegalTransition { from: TaskState, to: TaskState },
}

impl fmt::Display for TaskStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "task not found"),
            Self::IllegalTransition { from, to } => {
                write!(f, "cannot move a task from {from:?} to {to:?}")
            }
        }
    }
}

impl std::error::Error for TaskStoreError {}
