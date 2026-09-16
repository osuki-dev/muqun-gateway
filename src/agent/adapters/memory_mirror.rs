use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::agent::domain::{
    AgentDomainEvent, AgentPart, AgentSessionId, AgentSessionInfo, FormRequest,
    PermissionRequest, TimelineItem,
};
use crate::agent::ports::mirror::{AgentSessionSnapshot, MirrorFuture, SessionMirrorPort};

const MAX_EVENT_LOG_SIZE: usize = 2000;

struct SessionState {
    info: AgentSessionInfo,
    timeline: BTreeMap<String, TimelineItem>,
    permissions: HashMap<String, PermissionRequest>,
    forms: HashMap<String, FormRequest>,
    event_log: VecDeque<AgentDomainEvent>,
    current_seq: u64,
}

impl SessionState {
    fn new(info: AgentSessionInfo) -> Self {
        Self {
            info,
            timeline: BTreeMap::new(),
            permissions: HashMap::new(),
            forms: HashMap::new(),
            event_log: VecDeque::new(),
            current_seq: 0,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.current_seq += 1;
        self.current_seq
    }

    fn push_event(&mut self, event: AgentDomainEvent) {
        if self.event_log.len() >= MAX_EVENT_LOG_SIZE {
            self.event_log.pop_front();
        }
        self.event_log.push_back(event);
    }
}

#[derive(Clone, Default)]
pub struct MemoryMirror {
    sessions: Arc<RwLock<HashMap<AgentSessionId, SessionState>>>,
}

impl MemoryMirror {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Direct helper to append streaming text/reasoning chunk to existing timeline item
    pub async fn append_text_delta(
        &self,
        asid: &AgentSessionId,
        item_id: &str,
        delta: &str,
        is_reasoning: bool,
    ) -> Option<u64> {
        let mut sessions = self.sessions.write().await;
        let state = sessions.get_mut(asid)?;

        if let Some(item) = state.timeline.get_mut(item_id) {
            match &mut item.part {
                AgentPart::Text { text } if !is_reasoning => {
                    text.push_str(delta);
                }
                AgentPart::Reasoning { text, .. } if is_reasoning => {
                    text.push_str(delta);
                }
                _ => {}
            }
            item.updated_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            return Some(state.current_seq);
        }
        None
    }
}

impl SessionMirrorPort for MemoryMirror {
    fn get_snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> MirrorFuture<'a, Option<AgentSessionSnapshot>> {
        Box::pin(async move {
            let sessions = self.sessions.read().await;
            let state = sessions.get(asid)?;

            Some(AgentSessionSnapshot {
                info: state.info.clone(),
                timeline: state.timeline.values().cloned().collect(),
                permissions: state.permissions.values().cloned().collect(),
                forms: state.forms.values().cloned().collect(),
                seq: state.current_seq,
            })
        })
    }

    fn get_events_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after_seq: u64,
    ) -> MirrorFuture<'a, Option<Vec<AgentDomainEvent>>> {
        Box::pin(async move {
            let sessions = self.sessions.read().await;
            let state = sessions.get(asid)?;

            if let Some(first) = state.event_log.front() {
                if after_seq < first.seq() && after_seq > 0 {
                    // Requested sequence has fallen off the ring buffer -> client must resync
                    return None;
                }
            }

            let events: Vec<AgentDomainEvent> = state
                .event_log
                .iter()
                .filter(|ev| ev.seq() > after_seq)
                .cloned()
                .collect();
            Some(events)
        })
    }

    fn update_session<'a>(&'a self, info: AgentSessionInfo) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = info.asid.clone();
            let state = sessions.entry(asid.clone()).or_insert_with(|| SessionState::new(info.clone()));
            state.info = info.clone();
            let seq = state.next_seq();

            let event = AgentDomainEvent::SessionUpdated {
                asid,
                info,
                seq,
            };
            state.push_event(event);
            seq
        })
    }

    fn upsert_timeline_items<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        items: Vec<TimelineItem>,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            if let Some(state) = sessions.get_mut(asid) {
                let seq = state.next_seq();
                let mut updated_items = Vec::new();

                for mut item in items {
                    item.seq = seq;
                    state.timeline.insert(item.id.clone(), item.clone());
                    updated_items.push(item);
                }

                let event = AgentDomainEvent::TimelineUpsert {
                    asid: asid.clone(),
                    items: updated_items,
                    seq,
                };
                state.push_event(event);
                seq
            } else {
                0
            }
        })
    }

    fn add_permission<'a>(&'a self, request: PermissionRequest) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = request.asid.clone();
            if let Some(state) = sessions.get_mut(&asid) {
                let seq = state.next_seq();
                state.permissions.insert(request.id.clone(), request.clone());

                let event = AgentDomainEvent::PermissionPending {
                    asid,
                    request,
                    seq,
                };
                state.push_event(event);
                seq
            } else {
                0
            }
        })
    }

    fn resolve_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            if let Some(state) = sessions.get_mut(asid) {
                state.permissions.remove(request_id);
                let seq = state.next_seq();

                let event = AgentDomainEvent::PermissionResolved {
                    asid: asid.clone(),
                    request_id: request_id.to_string(),
                    seq,
                };
                state.push_event(event);
                seq
            } else {
                0
            }
        })
    }

    fn add_form<'a>(&'a self, request: FormRequest) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            let asid = request.asid.clone();
            if let Some(state) = sessions.get_mut(&asid) {
                let seq = state.next_seq();
                state.forms.insert(request.id.clone(), request.clone());

                let event = AgentDomainEvent::FormPending {
                    asid,
                    request,
                    seq,
                };
                state.push_event(event);
                seq
            } else {
                0
            }
        })
    }

    fn resolve_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
    ) -> MirrorFuture<'a, u64> {
        Box::pin(async move {
            let mut sessions = self.sessions.write().await;
            if let Some(state) = sessions.get_mut(asid) {
                state.forms.remove(form_id);
                let seq = state.next_seq();

                let event = AgentDomainEvent::FormResolved {
                    asid: asid.clone(),
                    form_id: form_id.to_string(),
                    seq,
                };
                state.push_event(event);
                seq
            } else {
                0
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::domain::{AgentPart, AgentSessionInfo, AgentSessionStatus, TimelineRole};

    #[tokio::test]
    async fn test_memory_mirror_lifecycle() {
        let mirror = MemoryMirror::new();
        let asid = AgentSessionId("ses-mirror-test".to_string());

        let info = AgentSessionInfo {
            asid: asid.clone(),
            backend_session_id: "ses-mirror-test".to_string(),
            title: "Test Session".to_string(),
            agent: Some("build".to_string()),
            model: None,
            status: AgentSessionStatus::Idle,
            directory: None,
            cost: None,
            tokens: None,
            updated_ms: 1000,
        };

        // 1. Upsert session
        let seq1 = mirror.update_session(info.clone()).await;
        assert_eq!(seq1, 1);

        // 2. Query snapshot
        let snap = mirror.get_snapshot(&asid).await.expect("snapshot should exist");
        assert_eq!(snap.info.title, "Test Session");
        assert_eq!(snap.timeline.len(), 0);

        // 3. Upsert timeline items
        let item1 = TimelineItem {
            id: "msg-1-part-0".to_string(),
            message_id: "msg-1".to_string(),
            role: TimelineRole::User,
            part: AgentPart::Text { text: "Hello".to_string() },
            seq: 0,
            updated_ms: 1001,
        };
        let seq2 = mirror.upsert_timeline_items(&asid, vec![item1]).await;
        assert_eq!(seq2, 2);

        // 4. Permissions pending & resolve
        let perm = PermissionRequest {
            id: "perm-1".to_string(),
            asid: asid.clone(),
            action: "execute".to_string(),
            resources: vec!["ls -la".to_string()],
            prompt: "List files".to_string(),
            tool: Some("bash".to_string()),
            message: None,
            options: vec![],
        };
        let seq3 = mirror.add_permission(perm).await;
        assert_eq!(seq3, 3);

        let snap_perm = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap_perm.permissions.len(), 1);

        let seq4 = mirror.resolve_permission(&asid, "perm-1").await;
        assert_eq!(seq4, 4);

        let snap_resolved = mirror.get_snapshot(&asid).await.unwrap();
        assert_eq!(snap_resolved.permissions.len(), 0);
    }
}

