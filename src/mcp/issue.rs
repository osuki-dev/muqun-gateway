//! Pane/task-bound token issuance and retirement of ended bindings.
//!
//! The helper a pane runs sends the pids of its own ancestry; the pane whose
//! shell is the nearest of them is the caller's pane. Pids are not a secret --
//! the endpoint is admin-only (the CLI's pairing token), and the match is what
//! binds the token to a place, not what authorises the request.
//! A shared daemon's helper instead names its immutable task. The same admin
//! credential authorizes issuance; task IDs are selectors, never credentials.

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use super::tokens::{IssuedToken, TokenBinding, PANE_TOKEN_TTL_MS};
use crate::backend::Pane;
use crate::tasks::domain::{now_ms, Task, TaskBinding, TaskId, TaskKind, TaskState};
use crate::{api_error, terminal_backend, AppState};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// How long a revoked or expired token row is kept before the sweep deletes
/// it. A pane's helper mints a token per connection, so dead rows would
/// otherwise pile up; a week still answers "expired" to a stale client.
const DEAD_TOKEN_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Deserialize)]
pub(crate) struct IssueRequest {
    /// The caller's own pid first, then its parent, and so on up.
    pub ancestor_pids: Option<Vec<u32>>,
    /// Admin-selected immutable task authority, never inferred from a daemon PID.
    pub task_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// The longest `label` accepted, in chars; it is stored and shown in the
/// phone's token list.
pub(crate) const MAX_LABEL_CHARS: usize = 64;
/// The most ancestor pids accepted; a real process chain is far shorter.
pub(crate) const MAX_ANCESTOR_PIDS: usize = 64;

impl IssueRequest {
    /// Whether the request is within [`MAX_LABEL_CHARS`] and
    /// [`MAX_ANCESTOR_PIDS`].
    pub(crate) fn within_bounds(&self) -> bool {
        let binding_valid = match (&self.ancestor_pids, &self.task_id) {
            (Some(pids), None) => pids.len() <= MAX_ANCESTOR_PIDS,
            (None, Some(id)) => valid_task_id(id),
            _ => false,
        };
        binding_valid
            && self
                .label
                .as_ref()
                .is_none_or(|label| label.chars().count() <= MAX_LABEL_CHARS)
    }
}

pub(crate) fn valid_task_id(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("t_")
        && id[2..]
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// Kept done/failed delegations may run explicit follow-ups. Cancellation or
/// deletion ends authority; another task in the same pane never inherits it.
pub(crate) async fn task_authority(state: &AppState, id: &str) -> anyhow::Result<Option<Task>> {
    Ok(state.tasks.task(&TaskId(id.to_owned())).await?.filter(|task| {
        task.kind == TaskKind::Delegation
            && task.state != TaskState::Cancelled
            && matches!(&task.binding, TaskBinding::Pane { agent_id: Some(agent), .. } if agent == "codex")
    }))
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaneMatch {
    pub session_id: String,
    pub pane_id: String,
}

/// The pane whose shell is the nearest of `ancestors` (given nearest first).
pub(crate) fn match_pane(ancestors: &[u32], panes: &[(String, Pane)]) -> Option<PaneMatch> {
    ancestors.iter().find_map(|pid| {
        panes
            .iter()
            .find(|(_, pane)| pane.shell_pid == Some(*pid))
            .map(|(session, pane)| PaneMatch {
                session_id: session.clone(),
                pane_id: pane.id.as_str().to_owned(),
            })
    })
}

/// Every pane of every configured session. A session whose backend cannot be
/// listed is left out of `panes` and named in `unreachable`.
struct Listing {
    panes: Vec<(String, Pane)>,
    unreachable: HashSet<String>,
}

async fn list_all(state: &AppState) -> Listing {
    let mut listing = Listing {
        panes: Vec::new(),
        unreachable: HashSet::new(),
    };
    for session in &state.config.sessions {
        match terminal_backend(session).list_panes().await {
            Ok(panes) => listing
                .panes
                .extend(panes.into_iter().map(|pane| (session.id.clone(), pane))),
            Err(error) => {
                tracing::debug!(session = %session.id, %error, "could not list panes");
                listing.unreachable.insert(session.id.clone());
            }
        }
    }
    listing
}

pub(crate) async fn issue_for_caller(
    state: &AppState,
    request: IssueRequest,
) -> Result<IssuedToken, (StatusCode, Json<Value>)> {
    if let Some(task_id) = request.task_id {
        if !matches!(task_authority(state, &task_id).await, Ok(Some(_))) {
            return Err(api_error(
                StatusCode::NOT_FOUND,
                "task_not_available",
                "Task token authority is unavailable",
            ));
        }
        return state
            .mcp_tokens
            .issue(
                TokenBinding::Task { task_id },
                request.label.unwrap_or_else(|| "codex task".into()),
                PANE_TOKEN_TTL_MS,
            )
            .await
            .map_err(|_| {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "token_store_unavailable",
                    "the assistant token store is unavailable",
                )
            });
    }
    let listing = list_all(state).await;
    let Some(found) = match_pane(
        request.ancestor_pids.as_deref().unwrap_or_default(),
        &listing.panes,
    ) else {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "pane_not_found",
            "No terminal pane belongs to the calling process",
        ));
    };
    let binding = TokenBinding::Pane {
        session_id: found.session_id,
        pane_id: found.pane_id,
    };
    let label = request.label.unwrap_or_else(|| "agent".to_owned());
    state
        .mcp_tokens
        .issue(binding, label, PANE_TOKEN_TTL_MS)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "could not issue an assistant token");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token_store_unavailable",
                "the assistant token store is unavailable",
            )
        })
}

/// Revoke the live pane tokens whose pane no longer exists. A session whose
/// backend cannot be listed keeps its tokens: an offline backend is not a
/// closed pane. Returns how many tokens were revoked.
///
/// A tmux session whose server is gone lists as empty (`list-panes` turning
/// "no server running" into `Ok(empty)`), so its tokens are revoked. That is
/// intended: a new server reuses `%N` pane ids, and a surviving token would
/// bind to whichever pane takes the old id.
pub(crate) async fn sweep_closed_panes(state: &AppState) -> usize {
    let Ok(rows) = state.mcp_tokens.list().await else {
        return 0;
    };
    let pane_tokens: Vec<_> = rows
        .into_iter()
        .filter_map(|row| match row.binding {
            TokenBinding::Pane {
                session_id,
                pane_id,
            } => Some((row.id, session_id, pane_id)),
            TokenBinding::Agent { .. } | TokenBinding::Task { .. } => None,
        })
        .collect();
    if pane_tokens.is_empty() {
        return 0;
    }
    let listing = list_all(state).await;
    let live: HashSet<(&str, &str)> = listing
        .panes
        .iter()
        .map(|(session, pane)| (session.as_str(), pane.id.as_str()))
        .collect();
    let mut revoked = 0;
    for (token_id, session_id, pane_id) in &pane_tokens {
        if listing.unreachable.contains(session_id)
            || live.contains(&(session_id.as_str(), pane_id.as_str()))
        {
            continue;
        }
        if matches!(state.mcp_tokens.revoke(token_id).await, Ok(true)) {
            revoked += 1;
        }
    }
    revoked
}

pub(crate) async fn sweep_task_tokens(state: &AppState) -> usize {
    let Ok(rows) = state.mcp_tokens.list().await else {
        return 0;
    };
    let mut revoked = 0;
    for row in rows {
        let TokenBinding::Task { task_id } = row.binding else {
            continue;
        };
        // Store failure does not prove a task was deleted.
        if matches!(task_authority(state, &task_id).await, Ok(None))
            && matches!(state.mcp_tokens.revoke(&row.id).await, Ok(true))
        {
            revoked += 1;
        }
    }
    revoked
}

pub(crate) fn spawn_sweeper(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let revoked = sweep_closed_panes(&state).await + sweep_task_tokens(&state).await;
            if revoked > 0 {
                tracing::info!(revoked, "revoked assistant tokens whose bindings ended");
            }
            let before = now_ms().saturating_sub(DEAD_TOKEN_RETENTION.as_millis() as u64);
            match state.mcp_tokens.purge_dead(before).await {
                Ok(0) => {}
                Ok(purged) => tracing::info!(purged, "deleted long-dead assistant tokens"),
                Err(error) => tracing::warn!(%error, "could not purge dead assistant tokens"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AgentStatus, PaneId, TabId, WorkspaceId};

    fn pane_with_pid(id: &str, pid: u32) -> Pane {
        Pane {
            id: PaneId::new(id),
            terminal_id: None,
            workspace_id: WorkspaceId::new("w"),
            tab_id: TabId::new("t"),
            label: None,
            terminal_title: None,
            cwd: None,
            focused: false,
            width: None,
            height: None,
            revision: None,
            foreground_command: None,
            agent: None,
            agent_status: AgentStatus::Unknown,
            max_offset_from_bottom: None,
            viewport_rows: None,
            alternate_on: None,
            cursor_x: None,
            cursor_y: None,
            shell_pid: Some(pid),
            history_identity: None,
        }
    }

    #[test]
    fn the_nearest_ancestor_that_owns_a_pane_wins() {
        let panes = vec![
            ("s".to_owned(), pane_with_pid("%1", 100)),
            ("s".to_owned(), pane_with_pid("%2", 300)),
        ];
        let m = match_pane(&[9000, 300, 100, 1], &panes).unwrap();
        assert_eq!(m.pane_id, "%2");
        assert!(match_pane(&[5, 6], &panes).is_none());
    }
}
