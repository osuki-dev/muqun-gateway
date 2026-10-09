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
    Updated { task: TaskView, seq: u64 },
}

impl TaskDomainEvent {
    pub(crate) fn seq(&self) -> u64 {
        match self {
            Self::Updated { seq, .. } => *seq,
        }
    }

    pub(crate) fn event_name(&self) -> &'static str {
        match self {
            Self::Updated { .. } => "task.updated",
        }
    }
}
