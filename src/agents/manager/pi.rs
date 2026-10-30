//! Pi child records folded into neutral domain events.
//!
//! The driver forwards every record its children print (and the replies it
//! sends itself) tagged with the session; [`handle_pi_record`] runs each
//! through the driver's live fold and turns the resulting [`LiveChange`]s
//! into mirror updates and `AgentDomainEvent`s.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::agents::adapters::memory_mirror::{MemoryMirror, SessionPatch};
use crate::agents::adapters::pi::mapper::LiveChange;
use crate::agents::adapters::pi::PiDriver;
use crate::agents::domain::{AgentDomainEvent, AgentSessionId};
use crate::agents::ports::agent::AgentPort;
use crate::agents::ports::mirror::SessionMirrorPort;

pub(crate) struct PiEventContext {
    pub driver: Arc<PiDriver>,
    pub mirror: Arc<MemoryMirror>,
    pub tx: broadcast::Sender<AgentDomainEvent>,
}

impl PiEventContext {
    pub(crate) fn emit(&self, mut event: AgentDomainEvent) {
        self.mirror.stamp_event(&mut event);
        // A send with no subscribers is not a failure: nobody is watching.
        let _ = self.tx.send(event);
    }

    /// The first status a session reports is often for one the mirror has
    /// never listed (a run started right after attach): give it the real
    /// title and directory rather than a placeholder.
    async fn ensure_session(&self, asid: &AgentSessionId) {
        if !self.mirror.is_placeholder(asid).await {
            return;
        }
        let Ok(mut info) = self.driver.get_session(&asid.0).await else {
            return;
        };
        let seq = self.mirror.update_session(info.clone()).await;
        self.mirror.overlay_read_markers(&mut info).await;
        self.emit(AgentDomainEvent::SessionUpdated {
            asid: asid.clone(),
            info: Box::new(info),
            seq,
        });
    }

    pub(crate) async fn abort_backlog(&self) {
        for (asid, changes) in self.driver.abort_backlog().await {
            for change in changes {
                apply_change(&asid, change, self).await;
            }
        }
        self.emit(AgentDomainEvent::Resync {
            asid: AgentSessionId(String::new()),
            reason: "event_backlog_overflow".into(),
        });
    }
}

pub(crate) async fn handle_pi_record(
    asid: &AgentSessionId,
    generation: u64,
    record: &Value,
    ctx: &PiEventContext,
) {
    let changes = ctx.driver.fold(asid, generation, record).await;
    let settled =
        record.get("type").and_then(Value::as_str) == Some("agent_settled") && !changes.is_empty();
    for change in changes {
        apply_change(asid, change, ctx).await;
    }
    if settled {
        if let Ok(mut info) = ctx.driver.get_session(&asid.0).await {
            let seq = ctx.mirror.update_session(info.clone()).await;
            ctx.mirror.overlay_read_markers(&mut info).await;
            ctx.emit(AgentDomainEvent::SessionUpdated {
                asid: asid.clone(),
                info: Box::new(info),
                seq,
            });
        }
    }
}

async fn apply_change(asid: &AgentSessionId, change: LiveChange, ctx: &PiEventContext) {
    match change {
        LiveChange::Model(model) => {
            let patch = SessionPatch {
                model: Some(model),
                ..SessionPatch::default()
            };
            if let Some((seq, mut info)) = ctx.mirror.merge_session_fields(asid, patch).await {
                ctx.mirror.overlay_read_markers(&mut info).await;
                ctx.emit(AgentDomainEvent::SessionUpdated {
                    asid: asid.clone(),
                    info: Box::new(info),
                    seq,
                });
            }
        }
        LiveChange::Status(status) => {
            ctx.ensure_session(asid).await;
            if let Some(seq) = ctx.mirror.update_status(asid, status, None).await {
                ctx.emit(AgentDomainEvent::StatusChanged {
                    asid: asid.clone(),
                    status,
                    error: None,
                    seq,
                });
            }
        }
        LiveChange::Upsert(items) => {
            if items.is_empty() {
                return;
            }
            // The live event must carry the same stamped item revisions as
            // replay/snapshots; older driver-local seqs cannot acknowledge
            // the App's optimistic follow-up row.
            ctx.emit(ctx.mirror.record_timeline_upsert(asid, items).await);
        }
        LiveChange::Remove(ids) => {
            if let Some((seq, ids)) = ctx.mirror.remove_timeline_items(asid, &ids).await {
                ctx.emit(AgentDomainEvent::TimelineRemoved {
                    asid: asid.clone(),
                    ids,
                    seq,
                });
            }
        }
        LiveChange::Permission(request) => {
            let request = *request;
            let seq = ctx.mirror.add_permission(request.clone()).await;
            ctx.emit(AgentDomainEvent::PermissionPending {
                asid: asid.clone(),
                request,
                seq,
            });
        }
        LiveChange::PermissionResolved(request_id) => {
            let seq = ctx.mirror.resolve_permission(asid, &request_id).await;
            ctx.emit(AgentDomainEvent::PermissionResolved {
                asid: asid.clone(),
                request_id,
                seq,
            });
        }
        LiveChange::Title(title) => {
            let patch = SessionPatch {
                title: Some(title),
                ..SessionPatch::default()
            };
            if let Some((seq, mut info)) = ctx.mirror.merge_session_fields(asid, patch).await {
                ctx.mirror.overlay_read_markers(&mut info).await;
                ctx.emit(AgentDomainEvent::SessionUpdated {
                    asid: asid.clone(),
                    info: Box::new(info),
                    seq,
                });
            }
        }
        LiveChange::Compaction {
            status,
            summary,
            reason,
            error,
        } => {
            ctx.emit(
                ctx.mirror
                    .record_compaction_with_error(asid, status, reason, summary, error)
                    .await,
            );
        }
        LiveChange::Usage { tokens, cost, .. } => {
            if ctx.driver.has_native_usage(&asid.0) {
                return;
            }
            if let Some((seq, info)) = ctx.mirror.update_usage(asid, cost, Some(tokens)).await {
                ctx.emit(AgentDomainEvent::SessionUpdated {
                    asid: asid.clone(),
                    info: Box::new(info),
                    seq,
                });
            }
        }
        // Answered by the driver's fold; nothing for the App.
        LiveChange::UiCancel(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::pi::{PiCommandLine, SessionStore};
    use crate::agents::domain::{AgentPart, AgentSessionStatus, TimelineItem, TimelineRole};

    #[tokio::test]
    async fn live_followup_revisions_match_snapshot_revisions_after_other_events() {
        let driver = Arc::new(PiDriver::new(
            PiCommandLine {
                binary: "unused".into(),
                extension: "unused".into(),
            },
            SessionStore::new(std::path::PathBuf::new()),
            true,
            std::time::Duration::from_secs(60),
            vec![],
        ));
        let mirror = Arc::new(MemoryMirror::for_agent("pi"));
        let (tx, mut events) = broadcast::channel(16);
        let ctx = PiEventContext {
            driver,
            mirror: mirror.clone(),
            tx,
        };
        let asid = AgentSessionId("session".into());
        for status in [
            AgentSessionStatus::Idle,
            AgentSessionStatus::Busy,
            AgentSessionStatus::Idle,
            AgentSessionStatus::Busy,
            AgentSessionStatus::Idle,
        ] {
            mirror.update_status(&asid, status, None).await;
        }
        let row = TimelineItem {
            id: "native-user".into(),
            message_id: "message".into(),
            role: TimelineRole::User,
            part: AgentPart::Text {
                text: "single send".into(),
            },
            seq: 1,
            updated_ms: 1,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        };
        apply_change(&asid, LiveChange::Upsert(vec![row]), &ctx).await;
        let AgentDomainEvent::TimelineUpsert { items, seq, .. } = events.recv().await.unwrap()
        else {
            panic!("timeline event expected");
        };
        assert!(seq > 1);
        assert_eq!(items[0].seq, seq);
        assert_eq!(
            mirror
                .get_timeline_item(&asid, "native-user")
                .await
                .unwrap()
                .seq,
            items[0].seq
        );
    }
}
