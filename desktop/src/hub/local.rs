//! [`LocalHub`]: the gateway's agents plane, in this process.
//!
//! Calls go where the gateway's own routes send them: the manager for the
//! agent or the session (`AgentRuntime::request_manager_for_*`, which wait,
//! bounded, for the supervisor's first attach pass so a call made at startup
//! is not refused), then its three use-case services. Nothing here adds a
//! use case the gateway does not have.

use std::path::Path;
use std::sync::Arc;

use muqun_gateway::agents::directories::{self, Completion};
use muqun_gateway::agents::manager::AgentManager;
use muqun_gateway::agents::ports::{AgentSessionSnapshot, FileDiffItem, SessionMirrorPort};
use muqun_gateway::agents::{
    AgentDomainEvent, AgentProject, AgentRuntime, AgentSessionId, AgentSessionInfo,
    PermissionDecision, SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use tokio::sync::broadcast;

use super::{HubError, HubFuture, SessionHub};

pub struct LocalHub {
    runtime: Arc<AgentRuntime>,
}

impl LocalHub {
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self { runtime }
    }

    async fn manager_for_agent(&self, agent: &str) -> Result<Arc<AgentManager>, HubError> {
        match self.runtime.request_manager_for_agent(agent).await {
            Some(manager) => Ok(manager),
            None => Err(self.unavailable(agent).await),
        }
    }

    async fn manager_for_session(
        &self,
        asid: &AgentSessionId,
    ) -> Result<Arc<AgentManager>, HubError> {
        self.runtime
            .request_manager_for_session(&asid.0)
            .await
            .ok_or_else(|| HubError::SessionNotFound(asid.0.clone()))
    }

    /// The gateway's own reason an agent is not attached, when it has one.
    async fn unavailable(&self, agent: &str) -> HubError {
        let reason = self
            .runtime
            .discovery_snapshot()
            .await
            .agents
            .into_iter()
            .find(|info| info.id == agent)
            .and_then(|info| info.reason);
        HubError::AgentUnavailable {
            agent: agent.to_string(),
            reason,
        }
    }
}

impl SessionHub for LocalHub {
    fn agents(&self) -> HubFuture<'_, Vec<AgentDiscoveryInfo>> {
        Box::pin(async move { Ok(self.runtime.discovery_snapshot().await.agents) })
    }

    fn attached(&self) -> HubFuture<'_, Vec<String>> {
        Box::pin(async move {
            let attached: Vec<String> = self
                .runtime
                .all_managers()
                .await
                .iter()
                .map(|m| m.agent().kind().to_string())
                .collect();
            Ok(super::AGENT_PREFERENCE
                .iter()
                .filter(|id| attached.iter().any(|a| a == *id))
                .map(|id| id.to_string())
                .collect())
        })
    }

    fn sessions<'a>(
        &'a self,
        agent: &'a str,
        query: SessionQuery,
    ) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            let sessions = manager.sessions().list_sessions(&query).await?;
            // As the list route does: a listed session is routed to its
            // agent, so opening it later needs no probe of every agent.
            for session in &sessions {
                self.runtime
                    .record_session_route(&session.asid.0, manager.agent().kind())
                    .await;
            }
            Ok(sessions)
        })
    }

    fn projects<'a>(&'a self, agent: &'a str) -> HubFuture<'a, Vec<AgentProject>> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            Ok(manager.agent().list_projects().await?)
        })
    }

    fn directories<'a>(&'a self, prefix: &'a str) -> HubFuture<'a, Completion> {
        Box::pin(
            async move { Ok(directories::complete(prefix, dirs::home_dir().as_deref()).await) },
        )
    }

    fn create<'a>(&'a self, agent: &'a str, cwd: &'a Path) -> HubFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            let directory = directories::expand_directory_param(Some(&cwd.to_string_lossy()));
            let info = manager
                .sessions()
                .create_session(directory.as_deref(), None, None)
                .await?;
            self.runtime
                .record_session_route(&info.asid.0, manager.agent().kind())
                .await;
            Ok(info)
        })
    }

    fn open<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            // An explicit foreground selection, as the App's `/prepare`: it
            // may restore a dormant native runtime. A refusal is not fatal --
            // the retained history can still be read.
            if let Err(err) = manager.sessions().prepare_session(asid).await {
                tracing::debug!(asid = %asid.0, %err, "session preparation failed; reading history anyway");
            }
            Ok(manager.sessions().get_snapshot(asid).await?)
        })
    }

    fn snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            // The mirror first: the session service's `get_snapshot` runs the
            // agent's `prepare_observed_session`, which for a terminal-held ACP
            // session that is not retained starts one more transcript
            // follower on every call (`acp/session.rs` `follow_terminal`).
            if let Some(snapshot) = manager.mirror().get_snapshot(asid).await {
                return Ok(snapshot);
            }
            Ok(manager.sessions().get_snapshot(asid).await?)
        })
    }

    fn earlier<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.sessions().load_earlier_timeline(asid).await?)
        })
    }

    fn prompt<'a>(&'a self, asid: &'a AgentSessionId, text: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.prompts().send_prompt(asid, text, &[], None).await?)
        })
    }

    fn interrupt<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.prompts().interrupt(asid).await?)
        })
    }

    fn reply_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        request_id: &'a str,
        decision: PermissionDecision,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager
                .interactions()
                .reply_permission(asid, request_id, decision, None)
                .await?)
        })
    }

    fn reply_form<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager
                .interactions()
                .reply_form(asid, form_id, answers)
                .await?)
        })
    }

    fn vcs_diff<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.sessions().get_vcs_diff(asid, "working").await?)
        })
    }

    fn events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.runtime.subscribe_events()
    }
}
