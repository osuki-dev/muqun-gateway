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

use super::{
    CatalogView, HubError, HubFuture, HubMode, PeerMention, SavedPermission, SessionHub, ShellRow,
    TaskDefaults, TaskRow, TimelineCatchUp, WorktreeRow,
};

/// How deep `descendants` walks below the session asked about, and how many
/// sessions it keeps (the gateway route's own bounds).
const MAX_DESCENDANT_DEPTH: usize = 8;
const MAX_DESCENDANTS: usize = 500;

pub struct LocalHub {
    runtime: Arc<AgentRuntime>,
}

/// Fill `image_assets` the HTTP routes would attach — Desktop never hits those.
pub(super) fn enrich_local_media(snapshot: &mut AgentSessionSnapshot) {
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
    fn mode(&self) -> HubMode {
        HubMode::Standalone
    }

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
            // Retained history paints immediately. Activating an ACP agent
            // (spawn + session/load + replay) can take many seconds; do that
            // in the background and let `acp_session_loaded` refresh.
            if let Some(mut snapshot) = manager.mirror().get_snapshot(asid).await {
                enrich_local_media(&mut snapshot);
                let manager = Arc::clone(&manager);
                let asid = asid.clone();
                tokio::spawn(async move {
                    if let Err(err) = manager.sessions().prepare_session(&asid).await {
                        tracing::debug!(
                            asid = %asid.0,
                            %err,
                            "background session preparation failed"
                        );
                    }
                });
                return Ok(snapshot);
            }
            // Cold open: nothing retained yet, so prepare must finish first.
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

    fn wake<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager.sessions().prepare_session(asid).await?;
            Ok(())
        })
    }

    fn peek<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Option<AgentSessionSnapshot>> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let Some(mut snapshot) = manager.mirror().get_snapshot(asid).await else {
                return Ok(None);
            };
            enrich_local_media(&mut snapshot);
            Ok(Some(snapshot))
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

    fn account_limits<'a>(&'a self, agent: &'a str) -> HubFuture<'a, serde_json::Value> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            let limits = manager
                .agent()
                .account_limits()
                .await
                .map_err(HubError::Agent)?;
            Ok(serde_json::to_value(limits).unwrap_or_default())
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
            // Prefer the agent (App parity). ACP/Cursor have no find_files;
            // empty or unsupported falls back to the local walk so `@` still
            // answers.
            if let Ok(manager) = self.manager_for_agent(agent).await {
                match manager
                    .agent()
                    .find_files(query, limit, Some(directory))
                    .await
                {
                    Ok(hits) if !hits.is_empty() => return Ok(hits),
                    Ok(_) => {}
                    Err(err) => tracing::debug!(agent, %err, "agent find_files; local fallback"),
                }
            }
            Ok(crate::platform::filesearch::find_files(directory, query, limit).await)
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

    fn auto_approve<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, bool> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager
                .agent()
                .auto_approve(&asid.0)
                .await?
                .unwrap_or(false))
        })
    }

    // The route's use case (`do_set_auto_approve`) minus its device gate:
    // in this process the owner is the only caller.
    fn set_auto_approve<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        enabled: bool,
    ) -> HubFuture<'a, bool> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager.agent().set_auto_approve(&asid.0, enabled).await?;
            Ok(manager
                .agent()
                .auto_approve(&asid.0)
                .await?
                .unwrap_or(false))
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

    fn view<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            Ok(manager.view_session(&asid.0, now).await?)
        })
    }

    fn stage_revert<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        message_id: &'a str,
        files: bool,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager
                .agent()
                .stage_revert(&asid.0, message_id, Some(files))
                .await?;
            Ok(())
        })
    }

    fn commit_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().commit_revert(&asid.0).await?)
        })
    }

    fn clear_revert<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            Ok(manager.agent().clear_revert(&asid.0).await?)
        })
    }

    fn compact<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        delivery: Option<&'a str>,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager.agent().compact_session(&asid.0, delivery).await?;
            Ok(())
        })
    }

    /// The gateway's `/descendants` walk: breadth first over every attached
    /// agent's children listing, bounded in depth and size; an agent that
    /// fails is skipped.
    fn descendants<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let managers = self.runtime.all_managers().await;
            let mut seen: std::collections::HashSet<String> =
                [asid.0.clone()].into_iter().collect();
            let mut found = Vec::new();
            let mut frontier = vec![asid.0.clone()];
            'walk: for _ in 0..MAX_DESCENDANT_DEPTH {
                let mut next = Vec::new();
                for parent in &frontier {
                    let query = SessionQuery {
                        parent_id: Some(parent.clone()),
                        ..SessionQuery::default()
                    };
                    for manager in &managers {
                        let Ok(children) = manager.sessions().list_sessions(&query).await else {
                            continue;
                        };
                        for child in children {
                            if !seen.insert(child.asid.0.clone()) {
                                continue;
                            }
                            if found.len() >= MAX_DESCENDANTS {
                                break 'walk;
                            }
                            self.runtime
                                .record_session_route(&child.asid.0, manager.agent().kind())
                                .await;
                            next.push(child.asid.0.clone());
                            found.push(child);
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
            found.sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
            Ok(found)
        })
    }

    fn list_saved_permissions<'a>(
        &'a self,
        asid: &'a AgentSessionId,
    ) -> HubFuture<'a, Vec<SavedPermission>> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let agent = manager.agent();
            let session = agent.get_session(&asid.0).await?;
            let Some(project_id) = session.project_id.filter(|p| !p.is_empty()) else {
                return Ok(Vec::new());
            };
            match agent.list_saved_permissions(Some(&project_id)).await {
                Ok(items) => Ok(items
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .filter(|p: &SavedPermission| !p.id.is_empty())
                    .collect()),
                Err(AgentError::Unsupported(_)) => Ok(Vec::new()),
                Err(err) => Err(err.into()),
            }
        })
    }

    fn forget_saved_permission<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        id: &'a str,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let agent = manager.agent();
            let session = agent.get_session(&asid.0).await?;
            let project_id = session
                .project_id
                .filter(|p| !p.is_empty())
                .ok_or_else(|| {
                    HubError::Agent(AgentError::InvalidRequest(
                        "the session reported no project".into(),
                    ))
                })?;
            let known = agent
                .list_saved_permissions(Some(&project_id))
                .await?
                .iter()
                .any(|item| item.get("id").and_then(|v| v.as_str()) == Some(id));
            if !known {
                return Err(HubError::Agent(AgentError::InvalidRequest(
                    "no such saved permission in this session's project".into(),
                )));
            }
            Ok(agent.delete_saved_permission(id).await?)
        })
    }

    fn list_shells<'a>(
        &'a self,
        agent: &'a str,
        directory: Option<&'a str>,
    ) -> HubFuture<'a, Vec<ShellRow>> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            match manager.agent().list_shells(directory).await {
                Ok(items) => Ok(items
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<ShellRow>(v).ok())
                    .filter(|s| !s.id.is_empty())
                    .collect()),
                Err(AgentError::Unsupported(_)) => Ok(Vec::new()),
                Err(err) => Err(err.into()),
            }
        })
    }

    fn kill_shell<'a>(&'a self, agent: &'a str, id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            manager.agent().kill_shell(id).await?;
            Ok(())
        })
    }

    fn background_session<'a>(&'a self, asid: &'a AgentSessionId) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            manager.agent().background_session(&asid.0).await?;
            manager.mirror().mark_running_tools_backgrounded(asid).await;
            Ok(())
        })
    }

    fn list_worktrees<'a>(
        &'a self,
        agent: &'a str,
        directory: &'a str,
    ) -> HubFuture<'a, Vec<WorktreeRow>> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            match manager.agent().list_worktrees(Some(directory)).await {
                Ok(items) => Ok(items
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<WorktreeRow>(v).ok())
                    .filter(|w| !w.directory.is_empty())
                    .collect()),
                Err(AgentError::Unsupported(_)) => Ok(Vec::new()),
                Err(err) => Err(err.into()),
            }
        })
    }

    fn remove_worktree<'a>(
        &'a self,
        agent: &'a str,
        project: &'a str,
        worktree: &'a str,
        force: bool,
    ) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            Ok(manager
                .agent()
                .remove_worktree(Some(project), worktree, Some(force))
                .await?)
        })
    }

    fn refresh_worktrees<'a>(&'a self, agent: &'a str, directory: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let manager = self.manager_for_agent(agent).await?;
            Ok(manager.agent().refresh_worktrees(Some(directory)).await?)
        })
    }

    fn timeline_after<'a>(
        &'a self,
        asid: &'a AgentSessionId,
        after: u64,
    ) -> HubFuture<'a, TimelineCatchUp> {
        Box::pin(async move {
            let manager = self.manager_for_session(asid).await?;
            let (mut items, _status, resync, latest_seq) =
                manager.mirror().get_timeline_delta(asid, after).await;
            let directory = manager.mirror().session_directory(&asid.0).await;
            muqun_gateway::agents::message_images::enrich_timeline(
                &asid.0,
                directory.as_deref(),
                &mut items,
            );
            Ok(TimelineCatchUp {
                items,
                latest_seq,
                resync,
            })
        })
    }

    // The task ledger belongs to the gateway service (`TaskService` is not
    // part of the embeddable agents plane), so a standalone desktop has none.
    fn list_tasks<'a>(
        &'a self,
        _root: Option<&'a str>,
        _state: Option<&'a str>,
    ) -> HubFuture<'a, Vec<TaskRow>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn create_task<'a>(
        &'a self,
        _goal: &'a str,
        _agent_id: Option<&'a str>,
        _cwd: Option<&'a str>,
        _peer_server_id: Option<&'a str>,
    ) -> HubFuture<'a, TaskRow> {
        Box::pin(async { Err(tasks_unsupported()) })
    }

    fn cancel_task<'a>(&'a self, _id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async { Err(tasks_unsupported()) })
    }

    fn complete_task<'a>(&'a self, _id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async { Err(tasks_unsupported()) })
    }

    fn task_defaults(&self) -> HubFuture<'_, TaskDefaults> {
        Box::pin(async { Ok(TaskDefaults::default()) })
    }

    fn list_peers(&self) -> HubFuture<'_, Vec<PeerMention>> {
        Box::pin(async move {
            // This computer always leads the `@` list. Linked fleet peers
            // need the gateway PeerStore on the desktop runtime (follow-up);
            // until then the popup still offers a real `@hostname` insert.
            Ok(vec![PeerMention {
                server_id: "self".into(),
                name: computer_name(),
                reachable: true,
                current: true,
                url: None,
            }])
        })
    }

    fn events(&self) -> broadcast::Receiver<AgentDomainEvent> {
        self.runtime.subscribe_events()
    }
}

fn tasks_unsupported() -> HubError {
    HubError::Agent(AgentError::Unsupported(
        "task delegation (needs the Muqun gateway service on this computer)".into(),
    ))
}

pub(super) fn computer_name() -> String {
    if let Ok(value) = std::env::var("HOSTNAME") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        if let Ok(name) = String::from_utf8(output.stdout) {
            let name = name.trim().trim_end_matches(".local").trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    "This computer".into()
}
