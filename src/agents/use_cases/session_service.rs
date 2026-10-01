use std::sync::Arc;

use crate::agents::domain::{
    AgentCatalog, AgentSessionId, AgentSessionInfo, ModelRef, SessionQuery,
};
use crate::agents::ports::agent::{AgentError, AgentPort, FileDiffItem};
use crate::agents::ports::mirror::{AgentSessionSnapshot, SessionMirrorPort};

pub struct SessionService {
    agent: Arc<dyn AgentPort>,
    mirror: Arc<dyn SessionMirrorPort>,
    /// The concrete mirror, for the few operations that are not part of the
    /// port because nothing else implements them.
    memory: Option<Arc<crate::agents::adapters::memory_mirror::MemoryMirror>>,
}

impl SessionService {
    pub fn new(agent: Arc<dyn AgentPort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self {
            agent,
            mirror,
            memory: None,
        }
    }

    /// Build a service that can also reach the in-memory mirror directly.
    pub fn with_memory_mirror(
        agent: Arc<dyn AgentPort>,
        mirror: Arc<crate::agents::adapters::memory_mirror::MemoryMirror>,
    ) -> Self {
        Self {
            agent,
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
    ) -> Result<Vec<AgentSessionInfo>, AgentError> {
        let mut sessions = self.agent.list_sessions(query).await?;
        for session in &mut sessions {
            self.stamp(session);
            if let Some(memory) = &self.memory {
                memory.overlay_read_markers(session).await;
            }
        }
        Ok(sessions)
    }

    /// Tag a session with the agent that owns it. The one place agent
    /// results gain `agent_id`; adapters and routes never set it.
    fn stamp(&self, info: &mut AgentSessionInfo) {
        info.agent_id = self.agent.kind().to_string();
    }

    /// One session as the agent reports it, tagged with its agent.
    pub async fn get_session(&self, asid: &str) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self.agent.get_session(asid).await?;
        self.stamp(&mut info);
        if let Some(memory) = &self.memory {
            memory.overlay_read_markers(&mut info).await;
        }
        Ok(info)
    }

    pub async fn create_session(
        &self,
        directory: Option<&str>,
        model: Option<&ModelRef>,
        mode: Option<&str>,
    ) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self.agent.create_session(directory, model, mode).await?;
        self.stamp(&mut info);
        self.mirror.update_session(info.clone()).await;
        Ok(info)
    }

    pub async fn get_snapshot(
        &self,
        asid: &AgentSessionId,
    ) -> Result<AgentSessionSnapshot, AgentError> {
        // A session the mirror holds is served from the mirror, even when its
        // timeline is legitimately empty: treating "no rows" as a cache miss
        // re-hit OpenCode twice on every poll of a new session.
        if let Some(snapshot) = self.mirror.get_snapshot(asid).await {
            return Ok(snapshot);
        }

        let mut info = self.agent.get_session(&asid.0).await?;
        self.stamp(&mut info);
        self.mirror.update_session(info.clone()).await;

        let timeline = self
            .agent
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
            .agent
            .get_pending_permissions(&asid.0)
            .await
            .unwrap_or_else(|err| {
                tracing::debug!(asid = %asid.0, %err, "pending permission catch-up failed");
                Vec::new()
            });
        let forms = self
            .agent
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
    ) -> Result<(), AgentError> {
        self.agent.switch_model(&asid.0, model).await
    }

    pub async fn get_catalog(&self, directory: Option<&str>) -> Result<AgentCatalog, AgentError> {
        self.agent.get_catalog(directory).await
    }

    pub async fn get_vcs_diff(
        &self,
        asid: &AgentSessionId,
        mode: &str,
    ) -> Result<Vec<FileDiffItem>, AgentError> {
        self.agent.get_vcs_diff(&asid.0, mode).await
    }

    pub async fn revert_session(
        &self,
        asid: &AgentSessionId,
        message_id: &str,
    ) -> Result<(), AgentError> {
        self.agent.revert_session(&asid.0, message_id).await?;
        let timeline = self
            .agent
            .get_timeline(&asid.0, 100)
            .await
            .unwrap_or_default();
        if !timeline.is_empty() {
            self.mirror.upsert_timeline_items(asid, timeline).await;
        }
        Ok(())
    }
}
