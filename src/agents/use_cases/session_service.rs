use std::sync::Arc;

use crate::agents::domain::{
    AgentCatalog, AgentSessionId, AgentSessionInfo, ModelRef, SessionQuery,
};
use crate::agents::ports::engine::{AgentEngineError, AgentEnginePort, FileDiffItem};
use crate::agents::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};

pub struct SessionService {
    engine: Arc<dyn AgentEnginePort>,
    mirror: Arc<dyn SessionMirrorPort>,
    /// The concrete mirror, for the few operations that are not part of the
    /// port because nothing else implements them.
    memory: Option<Arc<crate::agents::adapters::memory_mirror::MemoryMirror>>,
}

impl SessionService {
    pub fn new(engine: Arc<dyn AgentEnginePort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self {
            engine,
            mirror,
            memory: None,
        }
    }

    /// Build a service that can also reach the in-memory mirror directly.
    pub fn with_memory_mirror(
        engine: Arc<dyn AgentEnginePort>,
        mirror: Arc<crate::agents::adapters::memory_mirror::MemoryMirror>,
    ) -> Self {
        Self {
            engine,
            mirror: mirror.clone(),
            memory: Some(mirror),
        }
    }

    async fn mirror_pending(
        &self,
        asid: &AgentSessionId,
        permissions: Vec<crate::agents::domain::PermissionRequest>,
        forms: Vec<crate::agents::domain::FormRequest>,
    ) {
        if let Some(ref memory) = self.memory {
            memory.replace_pending(asid, permissions, forms).await;
        }
    }

    pub async fn list_sessions(
        &self,
        query: &SessionQuery,
    ) -> Result<Vec<AgentSessionInfo>, AgentEngineError> {
        let mut sessions = self.engine.list_sessions(query).await?;
        for session in &mut sessions {
            self.stamp(session);
        }
        Ok(sessions)
    }

    /// Tag a session with the harness that owns it. The one place engine
    /// results gain `harness`; adapters and routes never set it.
    fn stamp(&self, info: &mut AgentSessionInfo) {
        info.harness = self.engine.kind().to_string();
    }

    /// One session as the engine reports it, tagged with its harness.
    pub async fn get_session(&self, asid: &str) -> Result<AgentSessionInfo, AgentEngineError> {
        let mut info = self.engine.get_session(asid).await?;
        self.stamp(&mut info);
        Ok(info)
    }

    pub async fn create_session(
        &self,
        directory: Option<&str>,
        model: Option<&ModelRef>,
        agent: Option<&str>,
    ) -> Result<AgentSessionInfo, AgentEngineError> {
        let mut info = self.engine.create_session(directory, model, agent).await?;
        self.stamp(&mut info);
        self.mirror.update_session(info.clone()).await;
        Ok(info)
    }

    pub async fn get_snapshot(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentEngineError> {
        // A session the mirror holds is served from the mirror, even when its
        // timeline is legitimately empty: treating "no rows" as a cache miss
        // re-hit OpenCode twice on every poll of a new session.
        if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
            return Ok(snapshot);
        }

        let mut info = self.engine.get_session(&asid.0).await?;
        self.stamp(&mut info);
        self.mirror.update_session(info.clone()).await;

        let timeline = self
            .engine
            .get_timeline(&asid.0, 100)
            .await
            .unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror
                .upsert_timeline_items(asid, timeline.clone())
                .await;
        }

        // Anything raised while the stream was down is read back here:
        // `/api/event` is volatile and drops what happened during a
        // disconnect, so a permission prompt could otherwise be lost for good.
        self.catch_up_pending(asid).await;

        if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
            Ok(snapshot)
        } else {
            Ok(AgentSessionSnapshot {
                info,
                timeline,
                permissions: Vec::new(),
                forms: Vec::new(),
                inbox: Vec::new(),
                seq: 1,
            })
        }
    }

    /// Re-read the permissions and forms OpenCode still considers pending.
    pub async fn catch_up_pending(&self, asid: &AgentSessionId) {
        let permissions = self
            .engine
            .get_pending_permissions(&asid.0)
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(asid = %asid.0, %err, "pending permission catch-up failed");
                Vec::new()
            });
        let forms = self
            .engine
            .get_pending_forms(&asid.0)
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(asid = %asid.0, %err, "pending form catch-up failed");
                Vec::new()
            });
        if permissions.is_empty() && forms.is_empty() {
            return;
        }
        self.mirror_pending(asid, permissions, forms).await;
    }

    pub async fn get_events_after(
        &self,
        asid: &AgentSessionId,
        after_seq: u64,
    ) -> Option<Vec<crate::agents::domain::AgentDomainEvent>> {
        self.mirror.get_events_after(asid, after_seq).await
    }

    pub async fn switch_model(
        &self,
        asid: &AgentSessionId,
        model: &ModelRef,
    ) -> Result<(), AgentEngineError> {
        self.engine.switch_model(&asid.0, model).await
    }

    pub async fn get_catalog(
        &self,
        directory: Option<&str>,
    ) -> Result<AgentCatalog, AgentEngineError> {
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
        let timeline = self
            .engine
            .get_timeline(&asid.0, 100)
            .await
            .unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror.upsert_timeline_items(asid, timeline).await;
        }
        Ok(())
    }
}
