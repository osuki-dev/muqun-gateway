use std::sync::Arc;

use crate::agent::domain::{
    AgentCatalog, AgentSessionId, AgentSessionInfo, ModelRef,
};
use crate::agent::ports::engine::{AgentEngineError, AgentEnginePort, FileDiffItem};
use crate::agent::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};

pub struct SessionService {
    engine: Arc<dyn AgentEnginePort>,
    mirror: Arc<dyn SessionMirrorPort>,
}

impl SessionService {
    pub fn new(engine: Arc<dyn AgentEnginePort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { engine, mirror }
    }

    pub async fn list_sessions(&self, directory: Option<&str>) -> Result<Vec<AgentSessionInfo>, AgentEngineError> {
        self.engine.list_sessions(directory).await
    }

    pub async fn create_session(
        &self,
        directory: Option<&str>,
        model: Option<&ModelRef>,
        agent: Option<&str>,
    ) -> Result<AgentSessionInfo, AgentEngineError> {
        let info = self.engine.create_session(directory, model, agent).await?;
        self.mirror.update_session(info.clone()).await;
        Ok(info)
    }

    pub async fn get_snapshot(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentEngineError> {
        if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
            if !snapshot.timeline.is_empty() {
                return Ok(snapshot);
            }
        }

        // Snapshot not yet cached or timeline empty -> load from engine
        let info = self.engine.get_session(&asid.0).await?;
        self.mirror.update_session(info.clone()).await;

        let timeline = self.engine.get_timeline(&asid.0, 100).await.unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror.upsert_timeline_items(asid, timeline.clone()).await;
        }

        if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
            Ok(snapshot)
        } else {
            Ok(AgentSessionSnapshot {
                info,
                timeline,
                permissions: Vec::new(),
                forms: Vec::new(),
                seq: 1,
            })
        }
    }

    pub async fn get_events_after(
        &self,
        asid: &AgentSessionId,
        after_seq: u64,
    ) -> Option<Vec<crate::agent::domain::AgentDomainEvent>> {
        self.mirror.get_events_after(asid, after_seq).await
    }

    pub async fn switch_model(&self, asid: &AgentSessionId, model: &ModelRef) -> Result<(), AgentEngineError> {
        self.engine.switch_model(&asid.0, model).await
    }

    pub async fn get_catalog(&self, directory: Option<&str>) -> Result<AgentCatalog, AgentEngineError> {
        self.engine.get_catalog(directory).await
    }

    pub async fn get_vcs_diff(
        &self,
        asid: &AgentSessionId,
        mode: &str,
    ) -> Result<Vec<FileDiffItem>, AgentEngineError> {
        self.engine.get_vcs_diff(&asid.0, mode).await
    }

    pub async fn revert_session(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
    ) -> Result<(), AgentEngineError> {
        self.engine.revert_session(&asid.0, message_id).await?;
        let timeline = self.engine.get_timeline(&asid.0, 100).await.unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror.upsert_timeline_items(asid, timeline).await;
        }
        Ok(())
    }
}

