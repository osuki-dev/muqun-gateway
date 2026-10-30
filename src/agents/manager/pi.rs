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
}

pub(crate) async fn handle_pi_record(asid: &AgentSessionId, record: &Value, ctx: &PiEventContext) {
    for change in ctx.driver.fold(asid, record).await {
        apply_change(asid, change, ctx).await;
    }
}

async fn apply_change(asid: &AgentSessionId, change: LiveChange, ctx: &PiEventContext) {
    match change {
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
            let seq = ctx.mirror.upsert_timeline_items(asid, items.clone()).await;
            ctx.emit(AgentDomainEvent::TimelineUpsert {
                asid: asid.clone(),
                items,
                seq,
            });
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
        LiveChange::Compaction { status, summary } => {
            let seq = ctx
                .mirror
                .record_compaction(asid, status, None, summary.clone())
                .await;
            ctx.emit(AgentDomainEvent::CompactionChanged {
                asid: asid.clone(),
                status,
                reason: None,
                delta: summary,
                seq,
            });
        }
        LiveChange::Usage { tokens, cost, .. } => {
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
