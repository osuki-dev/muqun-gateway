//! The task entity: what the gateway was asked to get done, where it stands and
//! how it got there. Persisted through the `TaskStore` port; driven by
//! `TaskService`.
pub(crate) mod domain;
pub(crate) mod driver;
pub(crate) mod events;
pub(crate) mod pane_agent;
pub(crate) mod ports;
pub(crate) mod routes;
pub(crate) mod run_command;
pub(crate) mod service;
pub(crate) mod store_sqlite;

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

/// Whether a key may be sent to (or output read from) a task's pane: it is
/// [`same_pane`], or -- for a task bound by id alone, as rows from before the
/// shell pid existed are -- the backend could not list its panes.
pub(crate) async fn may_touch(
    backend: &dyn TerminalBackend,
    pane_id: &PaneId,
    shell_pid: Option<u32>,
) -> bool {
    let same = match backend.list_panes().await {
        Ok(panes) => same_pane(&panes, pane_id.as_str(), shell_pid),
        Err(_) => None,
    };
    same == Some(true) || (same.is_none() && shell_pid.is_none())
}
