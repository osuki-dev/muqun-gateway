//! What the task service tells listeners after every change it records.

use serde::Serialize;

use super::service::TaskView;

/// The broadcast channel's capacity; a listener that falls further behind
/// skips ahead.
pub(crate) const TASK_EVENT_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum TaskDomainEvent {
    #[serde(rename = "task.updated")]
    Updated { task: Box<TaskView>, seq: u64 },
    /// The device deleted an ended task: it is gone from the ledger.
    #[serde(rename = "task.deleted")]
    Deleted { task_id: String, seq: u64 },
}

impl TaskDomainEvent {
    pub(crate) fn seq(&self) -> u64 {
        match self {
            Self::Updated { seq, .. } | Self::Deleted { seq, .. } => *seq,
        }
    }

    pub(crate) fn event_name(&self) -> &'static str {
        match self {
            Self::Updated { .. } => "task.updated",
            Self::Deleted { .. } => "task.deleted",
        }
    }

    pub(crate) fn task_id(&self) -> &str {
        match self {
            Self::Updated { task, .. } => &task.task_id,
            Self::Deleted { task_id, .. } => task_id,
        }
    }
}
