use std::future::Future;
use std::pin::Pin;
use serde::{Deserialize, Serialize};

use crate::agent::domain::{
    AgentDomainEvent, AgentSessionId, AgentSessionInfo, FormRequest, PermissionRequest, TimelineItem,
};

pub type MirrorFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionSnapshot {
    pub info: AgentSessionInfo,
    pub timeline: Vec<TimelineItem>,
    pub permissions: Vec<PermissionRequest>,
    pub forms: Vec<FormRequest>,
    pub seq: u64,
}

pub trait SessionMirrorPort: Send + Sync {
    /// Get snapshot for a session if cached
    fn get_snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> MirrorFuture<'a, Option<AgentSessionSnapshot>>;

    /// Get events for a session strictly after `after_seq`
    fn get_events_after<'a>(&'a self, asid: &'a AgentSessionId, after_seq: u64) -> MirrorFuture<'a, Option<Vec<AgentDomainEvent>>>;

    /// Update session info
    fn update_session<'a>(&'a self, info: AgentSessionInfo) -> MirrorFuture<'a, u64>;

    /// Upsert timeline items
    fn upsert_timeline_items<'a>(&'a self, asid: &'a AgentSessionId, items: Vec<TimelineItem>) -> MirrorFuture<'a, u64>;

    /// Add a pending permission request
    fn add_permission<'a>(&'a self, request: PermissionRequest) -> MirrorFuture<'a, u64>;

    /// Resolve a pending permission request
    fn resolve_permission<'a>(&'a self, asid: &'a AgentSessionId, request_id: &'a str) -> MirrorFuture<'a, u64>;

    /// Add a pending form request
    fn add_form<'a>(&'a self, request: FormRequest) -> MirrorFuture<'a, u64>;

    /// Resolve a pending form request
    fn resolve_form<'a>(&'a self, asid: &'a AgentSessionId, form_id: &'a str) -> MirrorFuture<'a, u64>;
}
