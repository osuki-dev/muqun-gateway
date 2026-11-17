//! Computer-scoped attention, independent of the selected workspace.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use muqun_gateway::agents::ports::AgentPendingSnapshot;
use muqun_gateway::agents::{AgentDomainEvent, AgentSessionInfo};
use tokio::sync::broadcast;

use super::inbox::{Inbox, Update};
use super::notify::{Attention, AttentionKind, Gate};
use super::projection::ConfigStore;
use crate::hub::{LocalGateway, RemoteHub, SessionHub};

#[derive(Debug, Clone)]
pub struct Pending {
    pub computer: String,
    pub asid: String,
    pub request: String,
    pub kind: AttentionKind,
    pub title: String,
}
impl Pending {
    pub fn key(&self) -> String {
        serde_json::to_string(&(&self.computer, &self.asid, &self.request)).unwrap()
    }
}

pub fn notice_key(computer: &str, asid: &str) -> String {
    serde_json::to_string(&(computer, asid)).unwrap()
}

#[derive(Debug, Clone)]
pub struct Change {
    pub computer: String,
    pub pending: Vec<Pending>,
    pub raised: Vec<Attention>,
    pub info: HashMap<String, AgentSessionInfo>,
}

#[derive(Default)]
struct Ledger {
    gate: Gate,
    pending: HashMap<(String, String), Pending>,
    info: HashMap<String, AgentSessionInfo>,
    seq: HashMap<String, u64>,
    revision: u64,
    changed: HashMap<String, u64>,
}
impl Ledger {
    fn event(&mut self, computer: &str, event: &AgentDomainEvent) -> Option<Attention> {
        let asid = &event.asid().0;
        let seq = event.seq();
        if seq > 0 && self.seq.get(asid).is_some_and(|old| seq <= *old) {
            return None;
        }
        if seq > 0 {
            self.seq.insert(asid.clone(), seq);
        }
        if !asid.is_empty() {
            self.revision += 1;
            self.changed.insert(asid.clone(), self.revision);
        }
        if let AgentDomainEvent::SessionUpdated { info, .. } = event {
            self.info.insert(asid.clone(), *info.clone());
        }
        let entry = match event {
            AgentDomainEvent::PermissionPending { request, .. } => {
                Some((format!("p:{}", request.id), AttentionKind::Approval))
            }
            AgentDomainEvent::FormPending { request, .. } => {
                Some((format!("f:{}", request.id), AttentionKind::Question))
            }
            AgentDomainEvent::PermissionResolved { request_id, .. } => {
                self.pending
                    .remove(&(asid.clone(), format!("p:{request_id}")));
                None
            }
            AgentDomainEvent::FormResolved { form_id, .. } => {
                self.pending.remove(&(asid.clone(), format!("f:{form_id}")));
                None
            }
            _ => None,
        };
        if let Some((request, kind)) = entry {
            self.pending.insert(
                (asid.clone(), request.clone()),
                Pending {
                    computer: computer.into(),
                    asid: asid.clone(),
                    request,
                    kind,
                    title: self
                        .info
                        .get(asid)
                        .map(|i| i.title.clone())
                        .unwrap_or_default(),
                },
            );
        }
        self.gate.admit(event, Instant::now())
    }
    fn snapshot(&mut self, computer: &str, snapshot: AgentPendingSnapshot) -> Vec<Attention> {
        let asid = snapshot.info.asid.0.clone();
        if self.seq.get(&asid).is_some_and(|seq| *seq > snapshot.seq) {
            return vec![];
        }
        let keys: HashSet<_> = snapshot
            .permission_ids
            .iter()
            .map(|p| format!("p:{p}"))
            .chain(snapshot.form_ids.iter().map(|f| format!("f:{f}")))
            .collect();
        self.pending
            .retain(|(session, key), _| session != &asid || keys.contains(key));
        self.info.insert(asid.clone(), snapshot.info);
        self.gate
            .reconcile(&asid, &snapshot.permission_ids, &snapshot.form_ids);
        let mut raised = Vec::new();
        for request in keys {
            let kind = if request.starts_with("p:") {
                AttentionKind::Approval
            } else {
                AttentionKind::Question
            };
            let key = (asid.clone(), request.clone());
            if !self.pending.contains_key(&key) {
                raised.push(Attention {
                    asid: asid.clone(),
                    kind,
                });
            }
            self.pending.insert(
                key,
                Pending {
                    computer: computer.into(),
                    asid: asid.clone(),
                    request,
                    kind,
                    title: self.info[&asid].title.clone(),
                },
            );
        }
        self.seq.insert(asid, snapshot.seq);
        raised
    }
    fn batch(
        &mut self,
        computer: &str,
        batch: Vec<AgentPendingSnapshot>,
        before_read: &HashMap<String, u64>,
    ) -> Vec<Attention> {
        let retained: HashSet<_> = batch.iter().map(|s| s.info.asid.0.clone()).collect();
        let removed: HashSet<_> = self
            .pending
            .keys()
            .filter(|(asid, _)| {
                !retained.contains(asid) && self.changed.get(asid) == before_read.get(asid)
            })
            .map(|(asid, _)| asid.clone())
            .collect();
        self.pending.retain(|(asid, _), _| !removed.contains(asid));
        for asid in removed {
            self.gate.reconcile(&asid, &[], &[]);
            self.info.remove(&asid);
            self.seq.remove(&asid);
        }
        batch
            .into_iter()
            .flat_map(|snapshot| self.snapshot(computer, snapshot))
            .collect()
    }
    fn publish(&self, computer: &str, raised: Vec<Attention>, inbox: &Inbox) {
        inbox.push(Update::FleetAttention(Box::new(Change {
            computer: computer.into(),
            pending: self.pending.values().cloned().collect(),
            raised,
            info: self.info.clone(),
        })));
    }
}

struct Entry {
    hub: Arc<dyn SessionHub>,
    task: tokio::task::AbortHandle,
    credentials: Option<(String, String, Option<String>, Option<String>)>,
}
#[derive(Default)]
pub struct Fleet {
    entries: Mutex<HashMap<String, Entry>>,
}
impl Fleet {
    pub fn insert(
        &self,
        computer: String,
        hub: Arc<dyn SessionHub>,
        inbox: Arc<Inbox>,
        config: &ConfigStore,
    ) {
        let credentials = config.computer_profiles().get(&computer).and_then(|p| {
            Some((
                p.url.clone()?,
                p.token.clone()?,
                p.device_id.clone(),
                p.transport_key.clone(),
            ))
        });
        let task = tokio::spawn(watch(computer.clone(), hub.clone(), inbox));
        self.entries.lock().unwrap().insert(
            computer,
            Entry {
                hub,
                task: task.abort_handle(),
                credentials,
            },
        );
    }
    pub fn hub(&self, computer: &str) -> Option<Arc<dyn SessionHub>> {
        self.entries
            .lock()
            .unwrap()
            .get(computer)
            .map(|e| e.hub.clone())
    }
    pub async fn maintain(self: Arc<Self>, config: Arc<ConfigStore>, inbox: Arc<Inbox>) {
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let profiles = config.computer_profiles();
            if self.hub("local").is_none() {
                if let Some(gateway) = crate::hub::discover_local_gateway().await {
                    if let Ok(hub) = RemoteHub::start(gateway) {
                        self.insert("local".into(), hub, inbox.clone(), &config);
                    }
                }
            }
            let mut entries = self.entries.lock().unwrap();
            entries.retain(|id, entry| {
                if id == "local"
                    || profiles.get(id).is_some_and(|p| {
                        p.url.as_deref().is_some_and(|s| !s.trim().is_empty())
                            && p.token.as_deref().is_some_and(|s| !s.trim().is_empty())
                    })
                {
                    return true;
                }
                entry.task.abort();
                inbox.push(Update::FleetAttention(Box::new(Change {
                    computer: id.clone(),
                    pending: vec![],
                    raised: vec![],
                    info: HashMap::new(),
                })));
                false
            });
            for (id, profile) in &profiles {
                let (Some(url), Some(token)) = (&profile.url, &profile.token) else {
                    continue;
                };
                if url.trim().is_empty() || token.trim().is_empty() {
                    continue;
                }
                let credentials = (
                    url.clone(),
                    token.clone(),
                    profile.device_id.clone(),
                    profile.transport_key.clone(),
                );
                if let Some(entry) = entries.get_mut(id) {
                    if entry.credentials.as_ref() == Some(&credentials) {
                        continue;
                    }
                }
                if let Ok(hub) = RemoteHub::start(LocalGateway {
                    url: url.clone(),
                    token: token.clone(),
                    device_id: profile.device_id.clone(),
                    transport_key: profile.transport_key.clone(),
                }) {
                    if let Some(old) = entries.remove(id) {
                        old.task.abort();
                    }
                    let task = tokio::spawn(watch(id.clone(), hub.clone(), inbox.clone()));
                    entries.insert(
                        id.clone(),
                        Entry {
                            hub,
                            task: task.abort_handle(),
                            credentials: Some(credentials),
                        },
                    );
                }
            }
        }
    }
}

struct Reconciliation {
    epoch: u64,
    before_read: HashMap<String, u64>,
    snapshots: Option<Vec<AgentPendingSnapshot>>,
}

async fn reconcile(
    hub: Arc<dyn SessionHub>,
    tx: tokio::sync::mpsc::Sender<Reconciliation>,
    epoch: u64,
    before_read: HashMap<String, u64>,
) {
    if let Ok(Ok(snapshots)) =
        tokio::time::timeout(Duration::from_secs(10), hub.pending_attention()).await
    {
        let _ = tx
            .send(Reconciliation {
                epoch,
                before_read,
                snapshots,
            })
            .await;
    }
}

async fn watch(computer: String, hub: Arc<dyn SessionHub>, inbox: Arc<Inbox>) {
    let mut events = hub.events();
    let mut ledger = Ledger::default();
    let (tx, mut snapshots) = tokio::sync::mpsc::channel(8);
    let mut epoch = 0;
    let mut read = tokio::spawn(reconcile(
        hub.clone(),
        tx.clone(),
        epoch,
        ledger.changed.clone(),
    ));
    let mut last_read = Instant::now();
    let mut unsupported_reported = false;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut resync = false;
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => {
                    if let AgentDomainEvent::Resync { reason, .. } = &event {
                        resync = true;
                        if reason == "connected" {
                            read.abort();
                            epoch += 1;
                            ledger.seq.clear();
                        }
                    }
                    let changed = matches!(event, AgentDomainEvent::PermissionPending { .. } | AgentDomainEvent::FormPending { .. } | AgentDomainEvent::PermissionResolved { .. } | AgentDomainEvent::FormResolved { .. });
                    let raised = ledger.event(&computer, &event).into_iter().collect();
                    if changed { ledger.publish(&computer, raised, &inbox); }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => resync = true,
                Err(broadcast::error::RecvError::Closed) => { read.abort(); return; }
            },
            Some(result) = snapshots.recv() => {
                if result.epoch != epoch { continue; }
                if let Some(batch) = result.snapshots {
                    unsupported_reported = false;
                    let raised = ledger.batch(&computer, batch, &result.before_read);
                    ledger.publish(&computer, raised, &inbox);
                } else if !unsupported_reported {
                    unsupported_reported = true;
                    inbox.push(Update::FleetAttentionUnavailable(computer.clone()));
                }
            },
            _ = tick.tick() => {
                if (resync || (!unsupported_reported && last_read.elapsed() >= Duration::from_secs(60))) && read.is_finished() {
                    resync = false;
                    last_read = Instant::now();
                    read = tokio::spawn(reconcile(hub.clone(), tx.clone(), epoch, ledger.changed.clone()));
                }
                let raised: Vec<_> = ledger.gate.finished(Instant::now()).into_iter().map(|asid| Attention { asid, kind: AttentionKind::Finished }).collect();
                if !raised.is_empty() { ledger.publish(&computer, raised, &inbox); }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::{AgentSessionId, FormRequest};

    fn snapshot(seq: u64, requests: &[&str]) -> AgentPendingSnapshot {
        AgentPendingSnapshot {
            info: serde_json::from_value(serde_json::json!({
                "asid":"same-session", "agent_id":"codex", "title":"Work",
                "status":"busy", "updated_ms":1
            }))
            .unwrap(),
            permission_ids: vec![],
            form_ids: requests.iter().map(|s| s.to_string()).collect(),
            seq,
        }
    }
    fn question(seq: u64, id: &str) -> AgentDomainEvent {
        AgentDomainEvent::FormPending {
            asid: AgentSessionId("same-session".into()),
            request: FormRequest {
                id: id.into(),
                asid: AgentSessionId("same-session".into()),
                title: "private prompt".into(),
                fields: vec![],
                nonblocking: false,
            },
            seq,
        }
    }

    #[test]
    fn startup_reconnect_and_live_replays_notify_once_and_resolutions_win() {
        let mut ledger = Ledger::default();
        assert_eq!(
            ledger.snapshot("mini", snapshot(3, &["one", "two"])).len(),
            2
        );
        assert!(ledger
            .snapshot("mini", snapshot(3, &["one", "two"]))
            .is_empty());
        assert!(ledger.event("mini", &question(4, "one")).is_none());
        ledger.event(
            "mini",
            &AgentDomainEvent::FormResolved {
                asid: AgentSessionId("same-session".into()),
                form_id: "one".into(),
                reason: None,
                seq: 5,
            },
        );
        assert_eq!(ledger.pending.len(), 1);
        ledger.snapshot("mini", snapshot(4, &["one", "two"]));
        assert_eq!(
            ledger.pending.len(),
            1,
            "a late HTTP reply cannot revive a resolved question"
        );
        // A restarted gateway may restart sequence numbering. Reconciliation
        // clears a missed resolution without re-notifying still-open requests.
        ledger.seq.clear();
        assert!(ledger.snapshot("mini", snapshot(1, &["two"])).is_empty());
        assert!(ledger.snapshot("mini", snapshot(2, &[])).is_empty());
        assert!(ledger.pending.is_empty());
        assert!(ledger.event("mini", &question(3, "new")).is_some());
    }

    #[test]
    fn identical_remote_request_ids_are_independent_and_never_copy_prompt_text() {
        let mut local = Ledger::default();
        let mut remote = Ledger::default();
        assert!(local.event("local", &question(1, "one")).is_some());
        assert!(remote.event("mini", &question(1, "one")).is_some());
        let local_prompt = local.pending.values().next().unwrap();
        let remote_prompt = remote.pending.values().next().unwrap();
        assert_ne!(local_prompt.key(), remote_prompt.key());
        assert_ne!(
            notice_key("local", &local_prompt.asid),
            notice_key("mini", &remote_prompt.asid)
        );
        assert!(remote_prompt.title.is_empty());
        remote.event(
            "mini",
            &AgentDomainEvent::FormResolved {
                asid: AgentSessionId("same-session".into()),
                form_id: "one".into(),
                reason: None,
                seq: 2,
            },
        );
        assert!(remote.pending.is_empty());
        assert_eq!(local.pending.len(), 1);
    }

    #[test]
    fn reconnect_removes_deleted_sessions_but_keeps_requests_arriving_during_the_read() {
        let mut ledger = Ledger::default();
        ledger.event("mini", &question(1, "old"));
        let before = ledger.changed.clone();
        ledger.batch("mini", vec![], &before);
        assert!(
            ledger.pending.is_empty(),
            "a deleted or no-longer-retained session has no actionable request"
        );
        let before = ledger.changed.clone();
        ledger.event("mini", &question(2, "new"));
        ledger.batch("mini", vec![], &before);
        assert_eq!(
            ledger.pending.len(),
            1,
            "an HTTP read started before the new live request cannot erase it"
        );
        let before = ledger.changed.clone();
        ledger.batch("mini", vec![snapshot(3, &[])], &before);
        assert!(ledger.pending.is_empty());
    }
}
