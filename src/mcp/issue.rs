//! Pane-bound token issuance: which pane is the calling process in, and the
//! sweep that retires a pane's token once the pane is gone.
//!
//! The helper a pane runs sends the pids of its own ancestry; the pane whose
//! shell is the nearest of them is the caller's pane. Pids are not a secret --
//! the endpoint is admin-only (the CLI's pairing token), and the match is what
//! binds the token to a place, not what authorises the request.

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::Value;

use super::tokens::{IssuedToken, TokenBinding, PANE_TOKEN_TTL_MS};
use crate::backend::Pane;
use crate::{api_error, terminal_backend, AppState};

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Deserialize)]
pub(crate) struct IssueRequest {
    /// The caller's own pid first, then its parent, and so on up.
    pub ancestor_pids: Vec<u32>,
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
        self.ancestor_pids.len() <= MAX_ANCESTOR_PIDS
            && self
                .label
                .as_ref()
                .is_none_or(|label| label.chars().count() <= MAX_LABEL_CHARS)
    }
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

pub(crate) async fn issue_for_process(
    state: &AppState,
    request: IssueRequest,
) -> Result<IssuedToken, (StatusCode, Json<Value>)> {
    let listing = list_all(state).await;
    let Some(found) = match_pane(&request.ancestor_pids, &listing.panes) else {
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
            TokenBinding::Agent { .. } => None,
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

pub(crate) fn spawn_sweeper(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let revoked = sweep_closed_panes(&state).await;
            if revoked > 0 {
                tracing::info!(revoked, "revoked assistant tokens of closed panes");
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
