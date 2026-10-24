//! Shared, bounded Home discovery. No transcript, terminal capture or command execution.
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::agents::domain::{AgentSessionInfo, SessionQuery};
use crate::AppState;

pub(crate) const CAPABILITY: &str = "home_overview_v1";
pub(crate) const REFRESH: Duration = Duration::from_secs(30);
const MAX_WORKSPACES: usize = 16;
const MAX_PANES: usize = 100;
const MAX_SESSIONS: usize = 50;

#[derive(Default)]
pub(crate) struct OverviewCache {
    // Holding this across collection coalesces simultaneous App and peer subscribers.
    value: Mutex<Option<(Instant, Value)>>,
}

impl OverviewCache {
    pub(crate) async fn read(&self, state: &AppState) -> Value {
        let mut cache = self.value.lock().await;
        if let Some((at, value)) = &*cache {
            if at.elapsed() < REFRESH {
                return value.clone();
            }
        }
        let value = tokio::time::timeout(Duration::from_secs(4), collect(state))
            .await
            .unwrap_or(Value::Null);
        let value = if value.to_string().len() > 128 * 1024 {
            Value::Null
        } else {
            value
        };
        // Cache failures as well: many subscribers must not multiply failing reads.
        *cache = Some((Instant::now(), value.clone()));
        value
    }
}

fn bounded(text: &str) -> String {
    text.chars().take(256).collect()
}

fn session_row(row: AgentSessionInfo) -> Value {
    let mut result = json!({"asid":row.asid, "agentId":row.agent_id,
        "title":bounded(&row.title), "status":row.status,
        "directory":row.directory.unwrap_or_default(), "updatedMs":row.updated_ms});
    if let Some(terminal) = row.runtime.and_then(|runtime| runtime.terminal) {
        result["terminal"] = json!({"sessionId":terminal.session_id, "paneId":terminal.pane_id,
            "instanceId":terminal.instance_id});
    }
    result
}

fn pane_rows(panes: Vec<crate::backend::Pane>, agents: &[crate::backend::Agent]) -> Vec<Value> {
    panes.into_iter().map(|pane| {
        let agent = agents.iter().find(|agent| agent.pane_id == pane.id);
        let label = agent.and_then(|a| a.display_agent.as_ref().or(a.kind.as_ref()));
        let name = pane.label.as_deref().filter(|s| !s.trim().is_empty())
            .or(pane.terminal_title.as_deref().filter(|s| !s.trim().is_empty()))
            .or(label.map(String::as_str)).unwrap_or(pane.id.as_str());
        let status = match agent.map(|a| &a.status) {
            Some(crate::backend::AgentStatus::Working | crate::backend::AgentStatus::Starting) => "working",
            Some(crate::backend::AgentStatus::Idle) => "idle",
            Some(crate::backend::AgentStatus::Blocked) => "blocked",
            Some(crate::backend::AgentStatus::Completed) => "done",
            _ => "unknown",
        };
        let mut row = json!({"id":pane.id.as_str(), "paneId":pane.id.as_str(), "name":bounded(name),
            "status":status, "hasAgent":agent.is_some()});
        if let Some(agent) = agent {
            if let Some(id) = &agent.instance_id { row["instanceId"] = json!(id); }
            if let Some(pid) = agent.shell_pid { row["shellPid"] = json!(pid); }
            if let Some(pending) = agent.pending_user_input { row["pendingUserInput"] = json!(pending); }
            if let Some(label) = label { row["agentLabel"] = json!(bounded(label)); }
        } else if let Some(cwd) = pane.cwd { row["cwd"] = json!(cwd); }
        row
    }).collect()
}

async fn collect(state: &AppState) -> Value {
    // Bound the workload before invoking providers. Overflow is explicit fallback,
    // never a truncated list masquerading as a complete inventory.
    if state.config.sessions.len() > MAX_WORKSPACES {
        return Value::Null;
    }
    let query = SessionQuery {
        parent_id: Some("null".into()),
        limit: Some(MAX_SESSIONS),
        order: Some("desc".into()),
        ..Default::default()
    };
    let managers = state.agent_runtime.all_managers().await;
    if managers.len() > 16 {
        return Value::Null;
    }
    let mut sessions = Vec::new();
    for manager in managers {
        let Ok(rows) = manager.sessions().list_sessions(&query).await else {
            return Value::Null;
        };
        for row in rows
            .into_iter()
            .filter(|row| !row.deleted && row.parent_id.is_none())
        {
            sessions.push(session_row(row));
        }
    }
    sessions.sort_by(|a, b| {
        b["updatedMs"]
            .as_u64()
            .cmp(&a["updatedMs"].as_u64())
            .then_with(|| a["agentId"].as_str().cmp(&b["agentId"].as_str()))
            .then_with(|| a["asid"].as_str().cmp(&b["asid"].as_str()))
    });
    sessions.truncate(MAX_SESSIONS);
    let mut workspaces = Vec::new();
    for session in &state.config.sessions {
        let backend = crate::terminal_backend(session);
        let (panes, agents) = tokio::join!(backend.list_panes(), crate::backend_agents(session));
        let (Ok(panes), Ok(agents)) = (panes, agents) else {
            return Value::Null;
        };
        if panes.len() > MAX_PANES || agents.len() > MAX_PANES {
            return Value::Null;
        }
        workspaces.push(json!({
            "sessionId": session.id,
            "agents": pane_rows(panes, &agents),
        }));
    }
    json!({"workspaces": workspaces, "sessions": sessions})
}

/// Independent topic on the authenticated agent socket. Slow discovery never blocks tasks.
pub(crate) async fn local(
    state: AppState,
    sender: tokio::sync::mpsc::Sender<Value>,
    peer_grant: Option<(String, String, String)>,
) {
    let generation = uuid::Uuid::new_v4().to_string();
    let mut revision = 0_u64;
    let mut previous = Value::Null;
    let mut refresh = tokio::time::interval(REFRESH);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            _ = refresh.tick() => {}
        }
        if let Some((reader, grant, audience)) = &peer_grant {
            if !super::home_grants::allowed(&state, reader, grant, audience).await {
                return;
            }
        }
        let allowed = match &peer_grant {
            None => true,
            Some((_, grant, _)) => super::home_grants::overview_allowed(&state, grant).await,
        };
        let overview = if allowed {
            state.home_overview.read(&state).await
        } else {
            Value::Null
        };
        // Recheck after the potentially slow collection before disclosing metadata.
        if let Some((reader, grant, audience)) = &peer_grant {
            if !super::home_grants::allowed(&state, reader, grant, audience).await {
                return;
            }
        }
        revision += 1;
        let mut frame = json!({"t":"home_overview", "source_id":state.config.server_id,
            "generation":generation, "revision":revision, "snapshot":revision == 1,
            "fresh": !overview.is_null()});
        if revision == 1 || overview != previous {
            frame["overview"] = overview.clone();
        }
        if sender.send(frame).await.is_err() {
            return;
        }
        previous = overview;
    }
}
