use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use super::domain::{ChildCounts, Task, TaskChange, TaskEvent, TaskFilter, TaskId, TaskState};

pub(crate) type TaskFuture<'a, T> = Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

pub(crate) trait TaskStore: Send + Sync {
    /// Assigns `seq` and appends the creation event.
    fn insert<'a>(&'a self, task: &'a Task) -> TaskFuture<'a, Task>;
    fn get<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>>;
    /// Newest updated first.
    fn list<'a>(&'a self, filter: TaskFilter) -> TaskFuture<'a, Vec<Task>>;
    /// Refuses illegal moves with `TaskStoreError::IllegalTransition`.
    fn transition<'a>(&'a self, id: &'a TaskId, change: TaskChange) -> TaskFuture<'a, Task>;
    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>>;
    /// Each of `ids`' direct children, counted by state; a task with none is
    /// absent from the map.
    fn child_counts<'a>(&'a self, ids: Vec<TaskId>)
        -> TaskFuture<'a, HashMap<TaskId, ChildCounts>>;
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
