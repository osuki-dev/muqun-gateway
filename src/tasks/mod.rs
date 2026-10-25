//! The task entity: what the gateway was asked to get done, where it stands and
//! how it got there. Persisted through the `TaskStore` port; driven by
//! `TaskService`.
pub(crate) mod domain;
pub(crate) mod driver;
pub(crate) mod events;
pub(crate) mod notices;
pub(crate) mod pane_agent;
pub(crate) mod ports;
pub(crate) mod routes;
pub(crate) mod run_command;
pub(crate) mod service;
pub(crate) mod store_sqlite;
pub(crate) mod transcript;

use crate::backend::{Pane, PaneId, TerminalBackend};

/// Whether `pane_id` in `panes` is still the pane a task was bound to: listed
/// and, when the task was bound with a shell pid, still running that shell --
/// tmux hands `%N` out again after a server restart, and a stranger's pane
/// must not be read, interrupted or settled as the task's. `None` when that
/// cannot be told: the listing does not give the pane's shell pid now.
pub(crate) fn same_pane(panes: &[Pane], pane_id: &str, shell_pid: Option<u32>) -> Option<bool> {
    let Some(pane) = panes.iter().find(|pane| pane.id.as_str() == pane_id) else {
        return Some(false);
    };
    match (shell_pid, pane.shell_pid) {
        (None, _) => Some(true),
        (Some(expected), Some(current)) => Some(expected == current),
        (Some(_), None) => None,
    }
}

/// Whether a key may be sent to (or output read from) a task's pane.
/// Obtain fresh proof for only this pane: enriching every pane's process
/// information adds unrelated work and makes ownership depend on that fanout.
/// An unavailable pane or missing proof for a bound shell never grants authority.
pub(crate) async fn may_touch(
    backend: &dyn TerminalBackend,
    pane_id: &PaneId,
    shell_pid: Option<u32>,
) -> bool {
    match backend.get_pane(pane_id).await {
        Ok(pane) => {
            same_pane(std::slice::from_ref(&pane), pane_id.as_str(), shell_pid) == Some(true)
        }
        Err(_) => false,
    }
}
