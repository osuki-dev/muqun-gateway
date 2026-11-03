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
use muqun_gateway::agents::ports::AgentError;
use muqun_gateway::agents::ports::{AgentSessionSnapshot, FileDiffItem, SessionMirrorPort};
use muqun_gateway::agents::{
    AgentDomainEvent, AgentProject, AgentRuntime, AgentSessionId, AgentSessionInfo, ModelRef,
    PermissionDecision, SessionQuery, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::platform::StoredUpload;
use tokio::sync::broadcast;

use super::{CatalogView, HubError, HubFuture, SessionHub};

pub struct LocalHub {
    runtime: Arc<AgentRuntime>,
}

/// Fill `image_assets` the HTTP routes would attach — Desktop never hits those.
fn enrich_local_media(snapshot: &mut AgentSessionSnapshot) {
    muqun_gateway::agents::message_images::enrich_timeline(
        &snapshot.info.asid.0,
        snapshot.info.directory.as_deref(),
        &mut snapshot.timeline,
    );
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
            .map_err(|_| HubError::SessionNotFound(asid.0.clone()))
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
            // Mirror first: `get_snapshot` may re-prepare an ACP follower.
            let mut snapshot = if let Some(snapshot) = manager.mirror().get_snapshot(asid).await {
                snapshot
            } else {
                manager.sessions().get_snapshot(asid).await?
            };
            enrich_local_media(&mut snapshot);
            Ok(snapshot)
        })
    }

    fn snapshot<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, AgentSessionSnapshot> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            // The mirror first: the session service's `get_snapshot` runs the
            // agent's `prepare_observed_session`, which for a terminal-held ACP
            // session that is not retained starts one more transcript
            // follower on every call (`acp/session.rs` `follow_terminal`).
            let mut snapshot = if let Some(snapshot) = manager.mirror().get_snapshot(asid).await {
                snapshot
            } else {
                manager.sessions().get_snapshot(asid).await?
            };
            enrich_local_media(&mut snapshot);
            Ok(snapshot)
        })
    }

    fn earlier<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let mut items = manager.sessions().load_earlier_timeline(asid).await?;
            let directory = match manager.mirror().get_snapshot(asid).await {
                Some(s) => s.info.directory,
                None => manager
                    .sessions()
                    .get_session(&asid.0)
                    .await
                    .ok()
                    .and_then(|info| info.directory),
            };
            muqun_gateway::agents::message_images::enrich_timeline(
                &asid.0,
                directory.as_deref(),
                &mut items,
            );
            Ok(items)
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

    fn send<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager
                .prompts()
                .send_prompt(asid, text, attachments, delivery)
                .await?)
        })
    }

    fn find_files<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
        query: &'a str,
        limit: usize,
    ) -> HubFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async move {
            // As the route: a workspace is an absolute folder that exists.
            if !Path::new(directory).is_absolute() || !Path::new(directory).is_dir() {
                return Ok(Vec::new());
            }
            let manager = self.manager_for_agent(agent).await?;
            Ok(manager
                .agent()
                .find_files(query, limit, Some(directory))
                .await?)
        })
    }

    fn catalog<'a>(
        &'a self,
        agent: &'a str,
        asid: Option<&'a AgentSessionId>,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, CatalogView> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            let catalog = manager
                .sessions()
                .get_catalog_for_session(asid.map(|a| a.0.as_str()), directory)
                .await?;
            Ok(CatalogView { catalog })
        })
    }

    fn run_command<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, serde_json::Value> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let name = name.trim().trim_start_matches('/');
            Ok(manager
                .agent()
                .execute_command(&asid.0, name, arguments, delivery)
                .await?)
        })
    }

    fn activate_skill<'a>(&'a self, asid: &'a AgentSessionId, skill: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().activate_skill(&asid.0, skill, None).await?)
        })
    }

    fn switch_model<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        model: &'a ModelRef,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.sessions().switch_model(asid, model).await?)
        })
    }

    fn switch_mode<'a>(&'a self, asid: &'a AgentSessionId, mode: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().switch_mode(&asid.0, mode).await?)
        })
    }

    fn rename<'a>(&'a self, asid: &'a AgentSessionId, title: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().rename_session(&asid.0, title).await?)
        })
    }

    fn delete<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager.agent().delete_session(&asid.0).await?;
            manager.mirror().remove_session(asid).await;
            Ok(())
        })
    }

    fn fork<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: Option<&'a str>,
    ) -> HubFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.sessions().fork_session(&asid.0, message_id).await?)
        })
    }

    fn inbox_cancel<'a>(&'a self, asid: &'a AgentSessionId, item: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().cancel_inbox_item(&asid.0, item).await?)
        })
    }

    fn inbox_delivery<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        item: &'a str,
        delivery: &'a str,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager
                .agent()
                .set_inbox_delivery(&asid.0, item, delivery)
                .await?)
        })
    }

    fn store_attachment<'a>(
        &'a self,
        name: &'a str,
        bytes: Vec<u8>,
    ) -> HubFuture<'a, StoredUpload> {
        Box::pin(async move {
            let name = name.to_string();
            // Sniffing and writing up to 64 MiB: not on a runtime thread.
            tokio::task::spawn_blocking(move || {
                muqun_gateway::platform::store_upload(&name, &bytes)
            })
            .await
            .map_err(|err| HubError::Agent(AgentError::InvalidRequest(err.to_string())))?
            .map_err(HubError::Upload)
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
