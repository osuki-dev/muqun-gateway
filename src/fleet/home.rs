//! Read-only Home task summaries. Uses the existing task ledger and event bus;
//! no terminal observation or transcript reads are started by this subscription.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::tasks::events::TaskDomainEvent;
use crate::tasks::service::TaskView;
use crate::AppState;

pub(crate) const CAPABILITY: &str = "home_task_summary_v1";
pub(crate) const COLLECTOR_CAPABILITY: &str = "home_task_collector_v1";
pub(crate) const MAX_SOURCES: usize = 16;
const MAX_TASKS: usize = 100;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub peer_id: String,
    pub grant_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Subscription {
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Only peer callers use a grant/audience, and only for the local source.
    pub grant_id: Option<String>,
    pub audience: Option<String>,
}

impl Subscription {
    pub(crate) fn valid(&self) -> bool {
        let id = |s: &str| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control);
        self.sources.len() <= MAX_SOURCES
            && self
                .sources
                .iter()
                .all(|s| id(&s.peer_id) && id(&s.grant_id))
            && self
                .sources
                .iter()
                .map(|s| &s.peer_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == self.sources.len()
            && self.grant_id.as_deref().is_none_or(id)
            && self.audience.as_deref().is_none_or(id)
    }
}

/// A bounded projection, never the full task goal, command output or transcript.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct Summary {
    pub task_id: String,
    pub state: crate::tasks::domain::TaskState,
    pub kind: crate::tasks::domain::TaskKind,
    pub title: String,
    pub progress: Option<String>,
    pub binding: Value,
    pub updated_ms: u64,
    pub parent_task_id: Option<String>,
    pub reopen_count: u64,
    pub children: Value,
    pub origin: crate::tasks::domain::TaskOrigin,
    pub requested_by: Value,
    pub reopened_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub client_ref: Option<String>,
    pub summary: Option<String>,
    pub peer_reachable: Option<bool>,
    pub reason: Option<String>,
}

fn bounded(text: &str) -> String {
    text.chars().take(256).collect()
}

impl From<TaskView> for Summary {
    fn from(task: TaskView) -> Self {
        Self {
            origin: task.origin,
            requested_by: serde_json::to_value(task.requested_by).unwrap_or(Value::Null),
            reopened_ms: task.reopened_ms,
            exit_code: task.exit_code,
            client_ref: task.client_ref,
            summary: task.summary.as_deref().map(bounded),
            peer_reachable: task.peer_reachable,
            reason: task.reason.map(str::to_owned),
            task_id: task.task_id,
            state: task.state,
            kind: task.kind,
            title: bounded(&task.title),
            progress: task.progress.as_deref().map(bounded),
            binding: serde_json::to_value(task.binding).unwrap_or(Value::Null),
            updated_ms: task.updated_ms,
            parent_task_id: task.parent_task_id,
            reopen_count: task.reopen_count,
            children: serde_json::to_value(task.children).unwrap_or(Value::Null),
        }
    }
}

fn trim(tasks: &mut BTreeMap<String, Summary>) {
    let mut ordered: Vec<_> = tasks
        .values()
        .map(|task| (task.updated_ms, task.task_id.clone()))
        .collect();
    ordered.sort();
    for (_, id) in ordered.into_iter().rev().skip(MAX_TASKS) {
        tasks.remove(&id);
    }
}

pub(crate) async fn local(
    state: AppState,
    sender: mpsc::Sender<Value>,
    peer_grant: Option<(String, String, String)>,
    app_id: String,
) {
    // Register before the snapshot. A lag during that read triggers a new snapshot.
    let mut events = state.task_events.subscribe();
    let generation = uuid::Uuid::new_v4().to_string();
    let mut revision = 0_u64;
    let mut tasks = BTreeMap::new();
    let mut previous = BTreeMap::new();
    let mut snapshot = true;
    let mut read = true;
    let mut previous_cwd: Option<Option<String>> = None;
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(60));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh.tick().await;
    loop {
        if let Some((reader, grant, audience)) = &peer_grant {
            if !super::home_grants::allowed(&state, reader, grant, audience).await {
                return;
            }
        }
        let owner = match &peer_grant {
            Some((reader, grant, audience)) => {
                match super::home_grants::grant_owner(&state, reader, grant, audience).await {
                    Some(owner) => owner,
                    None => return,
                }
            }
            None => app_id.clone(),
        };
        let cwd = state
            .tasks
            .default_cwd(&crate::tasks::domain::TaskPrincipal {
                kind: crate::tasks::domain::PrincipalKind::Device,
                id: owner,
            })
            .await;
        if read {
            let Ok(list) = state.tasks.list_for(None, None, MAX_TASKS).await else {
                return;
            };
            tasks = list
                .into_iter()
                .map(Summary::from)
                .map(|task| (task.task_id.clone(), task))
                .collect();
        }
        if snapshot || tasks != previous || previous_cwd.as_ref() != Some(&cwd) {
            revision += 1;
            let changed: Vec<_> = tasks
                .iter()
                .filter(|(id, task)| snapshot || previous.get(*id) != Some(*task))
                .map(|(_, task)| task)
                .collect();
            let removed: Vec<_> = previous
                .keys()
                .filter(|id| !tasks.contains_key(*id))
                .collect();
            let frame = json!({"t":"home", "source_id": state.config.server_id,
                "generation":generation, "revision":revision, "snapshot":snapshot,
                "tasks":changed, "removed":removed, "limit":MAX_TASKS, "default_cwd":cwd});
            if sender.send(frame).await.is_err() {
                return;
            }
            previous = tasks.clone();
            previous_cwd = Some(cwd);
        }
        snapshot = false;
        read = false;
        tokio::select! {
            _ = sender.closed() => return,
            _ = refresh.tick() => { read = true; },
            event = events.recv() => match event {
                Ok(TaskDomainEvent::Updated { task, .. }) => {
                    let next = Summary::from(task);
                    if tasks.get(&next.task_id).is_none_or(|old| old.updated_ms <= next.updated_ms) {
                        tasks.insert(next.task_id.clone(), next);
                        trim(&mut tasks);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => { snapshot = true; read = true; },
                Err(_) => return,
            }
        }
    }
}

/// One-hop collection; the peer is never asked to collect its own peers.
pub(crate) async fn remote(
    state: AppState,
    source: Source,
    audience: String,
    sender: mpsc::Sender<Value>,
) {
    let Some(peer) = state.peers.get(&source.peer_id).await else {
        return;
    };
    let source_id = peer.server_id.clone();
    let status = |value: &str| json!({"t":"home_status","source_id":source_id,"status":value});
    let Some(peer) = state.peers.checked(&state, peer).await else {
        let _ = sender.send(status("unavailable")).await;
        return;
    };
    if !peer.capabilities.iter().any(|c| c == CAPABILITY) {
        let _ = sender.send(status("unsupported")).await;
        return;
    }
    let Ok(client) = super::peer_client::PeerClient::for_peer(&peer) else {
        return;
    };
    let request =
        json!({"t":"subscribe_home", "home":{"grant_id":source.grant_id,"audience":audience}});
    let Ok(mut feed) = client.subscribe_home(&request.to_string()).await else {
        let _ = sender.send(status("unavailable")).await;
        return;
    };
    state.peers.mark_seen(&source.peer_id).await;
    let mut recheck = tokio::time::interval(crate::STREAM_DEVICE_RECHECK_INTERVAL);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            _ = recheck.tick() => {
                let current = state.peers.get(&source.peer_id).await;
                if !current.is_some_and(|current| !current.unauthorized && current.device_id == peer.device_id && current.token == peer.token) { break; }
                if feed.heard().elapsed() < std::time::Duration::from_secs(50) {
                    state.peers.mark_seen(&source.peer_id).await;
                }
            }
            frame = feed.next_home() => {
                let Some(mut frame) = frame else { break; };
                // Bind origin to the paired link, never to the peer's frame label.
                frame["source_id"] = Value::String(source_id.clone());
                if sender.send(frame).await.is_err() { return; }
            }
        }
    }
    let _ = sender.send(status("unavailable")).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_bounded_and_cannot_duplicate_a_peer() {
        let source = Source {
            peer_id: "peer".into(),
            grant_id: "grant".into(),
        };
        assert!(!Subscription {
            sources: vec![source.clone(), source],
            ..Default::default()
        }
        .valid());
        assert!(!Subscription {
            audience: Some("\n".into()),
            ..Default::default()
        }
        .valid());
        assert!(Subscription::default().valid());
    }
}
