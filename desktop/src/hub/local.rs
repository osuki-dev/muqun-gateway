//! [`LocalHub`]: the gateway's agents plane, in this process.
//!
//! Calls go where the gateway's own routes send them: the manager for the
//! agent or the session (`AgentRuntime::request_manager_for_*`, which wait,
//! bounded, for the supervisor's first attach pass so a call made at startup
//! is not refused), then its three use-case services. Nothing here adds a
//! use case the gateway does not have.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use muqun_gateway::agents::directories::{self, Completion};
use muqun_gateway::agents::manager::AgentManager;
use muqun_gateway::agents::ports::AgentError;
use muqun_gateway::agents::ports::{AgentSessionSnapshot, FileDiffItem, SessionMirrorPort};
use muqun_gateway::agents::{
    AgentDomainEvent, AgentPart, AgentProject, AgentRuntime, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, ModelRef, PermissionDecision, SessionQuery, TimelineItem, TimelineRole,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use muqun_gateway::platform::StoredUpload;
use tokio::sync::broadcast;

use super::{
    CatalogView, HubError, HubFuture, HubMode, PeerMention, SavedPermission, SessionHub, ShellRow,
    TaskChildren, TaskDefaults, TaskHubEvent, TaskRow, TaskStep, TimelineCatchUp, WorktreeRow,
};

/// How deep `descendants` walks below the session asked about, and how many
/// sessions it keeps (the gateway route's own bounds).
const MAX_DESCENDANT_DEPTH: usize = 8;
const MAX_DESCENDANTS: usize = 500;
const TASK_EVENT_CAPACITY: usize = 512;

pub struct LocalHub {
    runtime: Arc<AgentRuntime>,
    tasks: Arc<LocalTaskManager>,
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
        Self {
            runtime,
            tasks: Arc::new(LocalTaskManager::new()),
        }
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

    fn list_tasks<'a>(
        &'a self,
        root: Option<&'a str>,
        state: Option<&'a str>,
    ) -> HubFuture<'a, Vec<TaskRow>> {
        Box::pin(async move {
            self.reconcile_tasks().await;
            Ok(self.tasks.list(root, state))
        })
    }

    fn create_task<'a>(
        &'a self,
        goal: &'a str,
        agent_id: Option<&'a str>,
        cwd: Option<&'a str>,
        _peer_server_id: Option<&'a str>,
    ) -> HubFuture<'a, TaskRow> {
        Box::pin(async move {
            let trimmed_goal = goal.trim();
            if trimmed_goal.is_empty() {
                return Err(HubError::Agent(AgentError::InvalidRequest(
                    "task goal must not be empty".into(),
                )));
            }

            let task_id = self.tasks.alloc_id();
            let base_dir = resolve_base_dir(cwd);

            let task_id_for_wt = task_id.clone();
            let base_for_wt = base_dir.clone();
            let (task_cwd, worktree_branch) = tokio::task::spawn_blocking(move || {
                prepare_task_worktree(&base_for_wt, &task_id_for_wt)
            })
            .await
            .unwrap_or_else(|_| (base_dir.clone(), None));

            let task_cwd_str = task_cwd.to_string_lossy().into_owned();
            let shell_cmd = parse_shell_command(trimmed_goal);
            let title = derive_task_title(shell_cmd.as_deref().unwrap_or(trimmed_goal));

            let chosen_agent = if shell_cmd.is_some() {
                None
            } else if let Some(explicit) = agent_id.map(str::trim).filter(|s| !s.is_empty()) {
                Some(explicit.to_owned())
            } else {
                let attached = self.attached().await.unwrap_or_default();
                Some(
                    attached
                        .into_iter()
                        .next()
                        .unwrap_or_else(|| "claude".to_owned()),
                )
            };

            let kind = if shell_cmd.is_some() {
                "command".to_owned()
            } else {
                "delegation".to_owned()
            };

            let pane_id = {
                let tid = task_id.clone();
                let tcwd = task_cwd.clone();
                tokio::task::spawn_blocking(move || create_daemon_session(&tid, &tcwd))
                    .await
                    .ok()
                    .flatten()
            };

            let mut bound_asid: Option<String> = None;
            let mut bound_manager: Option<(Arc<AgentManager>, AgentSessionId)> = None;

            if let Some(ref agent_name) = chosen_agent {
                if let Some(manager) = self.runtime.request_manager_for_agent(agent_name).await {
                    if let Ok(info) = manager
                        .sessions()
                        .create_session(Some(&task_cwd_str), None, None)
                        .await
                    {
                        self.runtime
                            .record_session_route(&info.asid.0, manager.agent().kind())
                            .await;
                        let _ = manager.agent().rename_session(&info.asid.0, &title).await;
                        bound_asid = Some(info.asid.0.clone());
                        bound_manager = Some((manager, info.asid));
                    }
                }
            }

            let now = now_ms();
            let initial_note = match (&chosen_agent, &bound_asid, &worktree_branch) {
                (Some(a), Some(asid), Some(br)) => {
                    Some(format!("Delegated to {a} ({asid}) on branch {br}"))
                }
                (Some(a), Some(asid), None) => Some(format!("Delegated to {a} ({asid})")),
                (Some(a), None, Some(br)) => {
                    Some(format!("Started {a} in terminal on branch {br}"))
                }
                (Some(a), None, None) => Some(format!("Started {a} in terminal")),
                (None, _, Some(br)) => Some(format!("Running command on branch {br}")),
                (None, _, None) => Some("Running command in terminal".into()),
            };

            let row = TaskRow {
                id: task_id.clone(),
                state: "working".into(),
                kind,
                goal: trimmed_goal.to_owned(),
                title: title.clone(),
                summary: None,
                progress: Some(if bound_asid.is_some() {
                    "Working…".into()
                } else {
                    "Running in terminal…".into()
                }),
                origin: "device".into(),
                parent_task_id: None,
                updated_ms: now,
                agent_id: chosen_agent.clone(),
                asid: bound_asid.clone(),
                pane_id: pane_id.clone(),
                worktree_branch: worktree_branch.clone(),
                cwd: Some(task_cwd_str.clone()),
                children: TaskChildren::default(),
                exit_code: None,
                started_ms: now,
                machine: None,
                completable: false,
            };

            self.tasks.insert(
                row.clone(),
                TaskStep {
                    at_ms: now,
                    to: "working".into(),
                    note: initial_note,
                },
            );

            if let Some((manager, asid)) = bound_manager {
                if let Some(ref pid) = pane_id {
                    let banner = build_terminal_banner(
                        &task_id,
                        &title,
                        &task_cwd_str,
                        worktree_branch.as_deref(),
                        Some(&asid.0),
                    );
                    let pid = pid.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = send_daemon_input(&pid, banner.as_bytes());
                    });
                }

                let prompt_text = format_delegation_prompt(
                    trimmed_goal,
                    &task_id,
                    &task_cwd_str,
                    worktree_branch.as_deref(),
                );
                let tasks = Arc::clone(&self.tasks);
                let tid = task_id.clone();
                tokio::spawn(async move {
                    if let Err(err) = manager
                        .prompts()
                        .send_prompt(&asid, &prompt_text, &[], None)
                        .await
                    {
                        tasks.transition_state(&tid, "failed", Some(err.to_string()), None);
                    }
                });
            } else if let Some(ref pid) = pane_id {
                let script = if let Some(ref cmd) = shell_cmd {
                    build_shell_task_script(
                        &task_id,
                        &title,
                        &task_cwd_str,
                        worktree_branch.as_deref(),
                        cmd,
                    )
                } else {
                    let agent_bin = chosen_agent.as_deref().unwrap_or("claude");
                    build_cli_agent_script(
                        &task_id,
                        &title,
                        &task_cwd_str,
                        worktree_branch.as_deref(),
                        agent_bin,
                        trimmed_goal,
                    )
                };
                let pid = pid.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = send_daemon_input(&pid, script.as_bytes());
                });
            }

            Ok(row)
        })
    }

    fn cancel_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            let Some(existing) = self.tasks.get(id) else {
                return Ok(());
            };
            if existing.is_ended() {
                return Ok(());
            }
            if let Some(ref asid_str) = existing.asid {
                let asid = AgentSessionId(asid_str.clone());
                if let Ok(manager) = self.manager_for_session(&asid).await {
                    let _ = manager.prompts().interrupt(&asid).await;
                }
            }
            if let Some(pane_id) = existing.pane_id.clone() {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ =
                        crate::term::daemon::SyncDaemonClient::new(None).close_session(&pane_id);
                })
                .await;
            }
            self.tasks
                .transition_state(id, "cancelled", Some("Cancelled by user".into()), None);
            Ok(())
        })
    }

    fn complete_task<'a>(&'a self, id: &'a str) -> HubFuture<'a, ()> {
        Box::pin(async move {
            self.tasks
                .transition_state(id, "done", Some("Confirmed done".into()), None);
            Ok(())
        })
    }

    fn task_defaults(&self) -> HubFuture<'_, TaskDefaults> {
        Box::pin(async {
            let default_cwd = std::env::current_dir()
                .ok()
                .filter(|p| p.is_dir())
                .or_else(dirs::home_dir)
                .map(|p| p.to_string_lossy().into_owned());
            Ok(TaskDefaults { default_cwd })
        })
    }

    fn task_history<'a>(&'a self, id: &'a str) -> HubFuture<'a, Vec<TaskStep>> {
        Box::pin(async move {
            self.reconcile_tasks().await;
            Ok(self.tasks.history(id))
        })
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

    fn task_events(&self) -> Option<broadcast::Receiver<TaskHubEvent>> {
        Some(self.tasks.subscribe())
    }
}

impl LocalHub {
    /// Reconcile active tasks with their bound ACP/RPC agent sessions and
    /// `term-daemon` PTY sessions.
    pub async fn reconcile_tasks(&self) {
        let open_tasks = self.tasks.open_tasks();
        if open_tasks.is_empty() {
            return;
        }

        let daemon_sessions = tokio::task::spawn_blocking(|| {
            crate::term::daemon::SyncDaemonClient::new(None)
                .list_sessions()
                .ok()
        })
        .await
        .ok()
        .flatten();

        for task in open_tasks {
            if let Some(ref asid_str) = task.asid {
                let asid = AgentSessionId(asid_str.clone());
                if let Ok(manager) = self.manager_for_session(&asid).await {
                    let snapshot = match manager.mirror().get_snapshot(&asid).await {
                        Some(s) => Some(s),
                        None => manager.sessions().get_snapshot(&asid).await.ok(),
                    };
                    if let Some(snap) = snapshot {
                        self.tasks.reconcile_snapshot(&task.id, &snap);
                        continue;
                    }
                }
            }

            if let Some(ref pane_id) = task.pane_id {
                let pid = pane_id.clone();
                let scrollback = tokio::task::spawn_blocking(move || read_daemon_scrollback(&pid))
                    .await
                    .ok()
                    .flatten();
                if let Some(ref text) = scrollback {
                    if self.tasks.reconcile_terminal_output(&task.id, text) {
                        continue;
                    }
                }
                if let Some(ref list) = daemon_sessions {
                    if let Some(info) = list.iter().find(|s| s.id == *pane_id) {
                        if !info.alive {
                            self.tasks.transition_state(
                                &task.id,
                                "done",
                                Some("Terminal session finished".into()),
                                Some(0),
                            );
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LocalTaskRecord {
    row: TaskRow,
    #[serde(default)]
    steps: Vec<TaskStep>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct PersistedTaskStore {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    tasks: Vec<LocalTaskRecord>,
}

/// Persistent in-process task ledger for [`LocalHub`].
pub struct LocalTaskManager {
    path: PathBuf,
    store: Mutex<PersistedTaskStore>,
    events_tx: broadcast::Sender<TaskHubEvent>,
}

impl Default for LocalTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalTaskManager {
    pub fn new() -> Self {
        Self::with_path(default_tasks_path())
    }

    pub fn with_path(path: PathBuf) -> Self {
        let mut store: PersistedTaskStore = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let max_num = store
            .tasks
            .iter()
            .filter_map(|t| t.row.id.strip_prefix("task-")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        store.next_id = store.next_id.max(max_num + 1).max(1);
        for rec in &mut store.tasks {
            rec.row.completable = rec.row.origin == "device"
                && matches!(rec.row.state.as_str(), "idle_unconfirmed" | "waiting");
        }
        let (events_tx, _) = broadcast::channel(TASK_EVENT_CAPACITY);
        Self {
            path,
            store: Mutex::new(store),
            events_tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TaskHubEvent> {
        self.events_tx.subscribe()
    }

    pub fn alloc_id(&self) -> String {
        let mut guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let num = guard.next_id.max(1);
        guard.next_id = num + 1;
        self.save_locked(&guard);
        format!("task-{num}")
    }

    pub fn insert(&self, mut row: TaskRow, step: TaskStep) {
        row.completable =
            row.origin == "device" && matches!(row.state.as_str(), "idle_unconfirmed" | "waiting");
        let mut guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.tasks.iter_mut().find(|r| r.row.id == row.id) {
            existing.row = row.clone();
            existing.steps.push(step);
        } else {
            guard.tasks.push(LocalTaskRecord {
                row: row.clone(),
                steps: vec![step],
            });
        }
        self.save_locked(&guard);
        drop(guard);
        let _ = self.events_tx.send(TaskHubEvent::Updated(Box::new(row)));
    }

    pub fn get(&self, id: &str) -> Option<TaskRow> {
        let guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .tasks
            .iter()
            .find(|r| r.row.id == id)
            .map(|r| r.row.clone())
    }

    pub fn open_tasks(&self) -> Vec<TaskRow> {
        let guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .tasks
            .iter()
            .filter(|r| !r.row.is_ended())
            .map(|r| r.row.clone())
            .collect()
    }

    pub fn history(&self, id: &str) -> Vec<TaskStep> {
        let guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .tasks
            .iter()
            .find(|r| r.row.id == id)
            .map(|r| r.steps.clone())
            .unwrap_or_default()
    }

    pub fn list(&self, root: Option<&str>, state: Option<&str>) -> Vec<TaskRow> {
        let guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let states: Option<Vec<&str>> = state
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(str::trim).collect());

        let all_rows: Vec<TaskRow> = guard.tasks.iter().map(|r| r.row.clone()).collect();
        let mut out: Vec<TaskRow> = all_rows
            .iter()
            .filter(|t| {
                if let Some(root_id) = root.filter(|r| !r.is_empty()) {
                    if t.id != root_id && t.parent_task_id.as_deref() != Some(root_id) {
                        return false;
                    }
                }
                if let Some(ref allowed) = states {
                    if !allowed.iter().any(|s| *s == t.state) {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();

        for row in &mut out {
            let mut children = TaskChildren::default();
            for candidate in &all_rows {
                if candidate.parent_task_id.as_deref() == Some(row.id.as_str()) {
                    children.total += 1;
                    match candidate.state.as_str() {
                        "queued" | "dispatching" | "working" => children.active += 1,
                        "waiting" | "idle_unconfirmed" => children.waiting += 1,
                        "failed" => children.failed += 1,
                        _ => {}
                    }
                }
            }
            row.children = children;
        }

        out.sort_by_key(|r| {
            (
                std::cmp::Reverse(r.updated_ms),
                std::cmp::Reverse(r.id.clone()),
            )
        });
        out
    }

    pub fn transition_state(
        &self,
        id: &str,
        new_state: &str,
        summary_or_note: Option<String>,
        exit_code: Option<i32>,
    ) -> Option<TaskRow> {
        let now = now_ms();
        let mut guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let rec = guard.tasks.iter_mut().find(|r| r.row.id == id)?;
        let state_changed = rec.row.state != new_state;
        rec.row.state = new_state.to_owned();
        rec.row.updated_ms = now;
        if let Some(ref s) = summary_or_note {
            if !s.trim().is_empty() {
                rec.row.summary = Some(s.trim().to_owned());
            }
        }
        if exit_code.is_some() {
            rec.row.exit_code = exit_code;
        }
        if matches!(
            new_state,
            "done" | "failed" | "cancelled" | "idle_unconfirmed" | "waiting"
        ) {
            rec.row.progress = None;
        }
        rec.row.completable =
            rec.row.origin == "device" && matches!(new_state, "idle_unconfirmed" | "waiting");
        if state_changed || rec.steps.is_empty() {
            rec.steps.push(TaskStep {
                at_ms: now,
                to: new_state.to_owned(),
                note: summary_or_note,
            });
        }
        let updated = rec.row.clone();
        self.save_locked(&guard);
        drop(guard);
        let _ = self
            .events_tx
            .send(TaskHubEvent::Updated(Box::new(updated.clone())));
        Some(updated)
    }

    pub fn update_progress(&self, id: &str, progress: Option<String>) {
        let mut guard = self.store.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rec) = guard.tasks.iter_mut().find(|r| r.row.id == id) else {
            return;
        };
        if rec.row.progress == progress {
            return;
        }
        rec.row.progress = progress;
        rec.row.updated_ms = now_ms();
        let updated = rec.row.clone();
        self.save_locked(&guard);
        drop(guard);
        let _ = self
            .events_tx
            .send(TaskHubEvent::Updated(Box::new(updated)));
    }

    /// Reconcile one open task against an `AgentSessionSnapshot`.
    pub fn reconcile_snapshot(&self, id: &str, snapshot: &AgentSessionSnapshot) {
        let Some(current) = self.get(id) else {
            return;
        };
        if current.is_ended() {
            return;
        }

        match snapshot.info.status {
            AgentSessionStatus::Busy | AgentSessionStatus::Retry => {
                let latest_progress =
                    extract_progress(&snapshot.timeline).or_else(|| Some("Working…".to_owned()));
                if current.state != "working" {
                    self.transition_state(id, "working", None, None);
                }
                self.update_progress(id, latest_progress);
            }
            AgentSessionStatus::Failed => {
                let reason = snapshot
                    .info
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .or_else(|| last_assistant_line(&snapshot.timeline))
                    .unwrap_or_else(|| "Agent session failed".to_owned());
                self.transition_state(id, "failed", Some(reason), None);
            }
            AgentSessionStatus::Interrupted => {
                self.transition_state(
                    id,
                    "idle_unconfirmed",
                    Some("Session interrupted".to_owned()),
                    None,
                );
            }
            AgentSessionStatus::Idle | AgentSessionStatus::Unknown => {
                let assistant_text = collect_assistant_text(&snapshot.timeline);
                if let Some((marker_state, note)) = parse_task_markers(&assistant_text) {
                    if current.state != marker_state || current.summary.as_deref() != Some(&note) {
                        self.transition_state(id, marker_state, Some(note), None);
                    }
                } else if let Some(last_line) = last_assistant_line(&snapshot.timeline) {
                    if current.state != "idle_unconfirmed"
                        || current.summary.as_deref() != Some(last_line.as_str())
                    {
                        self.transition_state(id, "idle_unconfirmed", Some(last_line), None);
                    }
                }
            }
        }
    }

    /// Reconcile a terminal-backed task against its scrollback text.
    /// Returns `true` if a terminal marker transitioned the task.
    pub fn reconcile_terminal_output(&self, id: &str, scrollback: &str) -> bool {
        let Some(current) = self.get(id) else {
            return false;
        };
        if current.is_ended() {
            return true;
        }
        if let Some((marker_state, note)) = parse_task_markers(scrollback) {
            let exit_code = match marker_state {
                "done" => Some(0),
                "failed" => Some(1),
                _ => None,
            };
            self.transition_state(id, marker_state, Some(note), exit_code);
            return true;
        }
        false
    }

    fn save_locked(&self, store: &PersistedTaskStore) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(bytes) = serde_json::to_vec_pretty(store) else {
            return;
        };
        let file_name = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("local-tasks.json");
        let tmp = self
            .path
            .with_file_name(format!(".{file_name}.tmp.{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn default_tasks_path() -> PathBuf {
    if let Some(config_dir) = dirs::config_dir() {
        config_dir
            .join(crate::platform::DIR_NAME)
            .join("local-tasks.json")
    } else {
        std::env::temp_dir().join(format!("{}-local-tasks.json", crate::platform::DIR_NAME))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn resolve_base_dir(cwd: Option<&str>) -> PathBuf {
    cwd.map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| directories::expand_directory_param(Some(s)))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok().filter(|p| p.is_dir()))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// If `base_dir` is inside a Git repository with at least one commit, creates an
/// isolated Git worktree at `<git_common_dir>/muqun-worktrees/<task_id>` on branch
/// `muqun/<task_id>`. Falls back to `(base_dir.to_path_buf(), None)` on any error.
pub(crate) fn prepare_task_worktree(base_dir: &Path, task_id: &str) -> (PathBuf, Option<String>) {
    let git_out = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(base_dir)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?;
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };

    let Some(toplevel) = git_out(&["rev-parse", "--show-toplevel"]) else {
        return (base_dir.to_path_buf(), None);
    };
    let Some(common_dir_raw) = git_out(&["rev-parse", "--git-common-dir"]) else {
        return (base_dir.to_path_buf(), None);
    };
    if git_out(&["rev-parse", "--verify", "HEAD"]).is_none() {
        return (base_dir.to_path_buf(), None);
    }

    let repo_root = PathBuf::from(&toplevel);
    let common_dir = {
        let p = PathBuf::from(&common_dir_raw);
        if p.is_absolute() {
            p
        } else {
            base_dir.join(p)
        }
    };

    let worktrees_root = common_dir.join("muqun-worktrees");
    if std::fs::create_dir_all(&worktrees_root).is_err() {
        return (base_dir.to_path_buf(), None);
    }

    let wt_path = worktrees_root.join(task_id);
    if wt_path.exists() {
        let _ = std::fs::remove_dir_all(&wt_path);
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "prune"])
            .output();
    }

    let branch = format!("muqun/{task_id}");
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "add", "-B", &branch])
        .arg(&wt_path)
        .arg("HEAD")
        .output();

    if matches!(status, Ok(out) if out.status.success()) && wt_path.is_dir() {
        (wt_path, Some(branch))
    } else {
        (base_dir.to_path_buf(), None)
    }
}

fn parse_shell_command(goal: &str) -> Option<String> {
    let trimmed = goal.trim();
    trimmed
        .strip_prefix('!')
        .or_else(|| trimmed.strip_prefix("$ "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn derive_task_title(text: &str) -> String {
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Task");
    let mut chars = first_line.chars();
    let head: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

fn format_delegation_prompt(
    goal: &str,
    task_id: &str,
    cwd: &str,
    worktree_branch: Option<&str>,
) -> String {
    let wt_line = match worktree_branch {
        Some(branch) => format!("Isolated Git worktree: `{cwd}` (branch `{branch}`)\n"),
        None => format!("Working directory: `{cwd}`\n"),
    };
    format!(
        "{goal}\n\n---\nTask ID: `{task_id}`\n{wt_line}\
         When you have completely finished this task, include `MUQUN_TASK_DONE: <one-line summary>` on its own line at the end of your reply.\n\
         If you are blocked and need user input before continuing, include `MUQUN_TASK_BLOCKED: <reason>` on its own line."
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn build_terminal_banner(
    task_id: &str,
    title: &str,
    cwd: &str,
    branch: Option<&str>,
    asid: Option<&str>,
) -> String {
    let branch_info = branch.unwrap_or("none");
    let session_info = asid.unwrap_or("terminal");
    format!(
        "printf '\\033[1;36m[Muqun Task %s]\\033[0m %s\\n\\033[2mFolder: %s | Branch: %s | Session: %s\\033[0m\\n' {} {} {} {} {}\n",
        shell_single_quote(task_id),
        shell_single_quote(title),
        shell_single_quote(cwd),
        shell_single_quote(branch_info),
        shell_single_quote(session_info),
    )
}

fn build_shell_task_script(
    task_id: &str,
    title: &str,
    cwd: &str,
    branch: Option<&str>,
    cmd: &str,
) -> String {
    let banner = build_terminal_banner(task_id, title, cwd, branch, None);
    format!(
        "{banner}({cmd}) && printf '\\n%s_%s: Command completed\\n' MUQUN_TASK DONE || printf '\\n%s_%s: Command exited with code %s\\n' MUQUN_TASK FAILED \"$?\"\n"
    )
}

fn build_cli_agent_script(
    task_id: &str,
    title: &str,
    cwd: &str,
    branch: Option<&str>,
    agent_bin: &str,
    goal: &str,
) -> String {
    let banner = build_terminal_banner(task_id, title, cwd, branch, None);
    let quoted_goal = shell_single_quote(goal);
    let inv = match agent_bin {
        "claude" => format!("claude -p {quoted_goal}"),
        "codex" => format!("codex exec {quoted_goal}"),
        "opencode" => format!("opencode run {quoted_goal}"),
        "gemini" => format!("gemini -p {quoted_goal}"),
        other => format!("{other} {quoted_goal}"),
    };
    format!(
        "{banner}({inv}) && printf '\\n%s_%s: Completed by {agent_bin}\\n' MUQUN_TASK DONE || printf '\\n%s_%s: {agent_bin} exited with code %s\\n' MUQUN_TASK FAILED \"$?\"\n"
    )
}

fn create_daemon_session(task_id: &str, cwd: &Path) -> Option<String> {
    let client = crate::term::daemon::SyncDaemonClient::new(None);
    client
        .create_session(Some(task_id.to_owned()), Some(cwd.to_path_buf()), 100, 30)
        .or_else(|_| {
            let _ = crate::term::daemon::ensure_daemon_running();
            client.create_session(Some(task_id.to_owned()), Some(cwd.to_path_buf()), 100, 30)
        })
        .ok()
}

fn send_daemon_input(pane_id: &str, data: &[u8]) -> std::io::Result<()> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let req = crate::term::daemon::Request::Input {
        id: pane_id.to_owned(),
        data: b64,
    };
    let _ = crate::term::daemon::SyncDaemonClient::new(None).send_request(&req)?;
    Ok(())
}

fn read_daemon_scrollback(pane_id: &str) -> Option<String> {
    let req = crate::term::daemon::Request::Attach {
        id: pane_id.to_owned(),
        raw: false,
    };
    let resp = crate::term::daemon::SyncDaemonClient::new(None)
        .send_request(&req)
        .ok()?;
    let b64 = resp.scrollback?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn extract_progress(timeline: &[TimelineItem]) -> Option<String> {
    for item in timeline.iter().rev() {
        match &item.part {
            AgentPart::Tool(tc) => {
                let label = tc
                    .title
                    .as_deref()
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Running {}", tc.name));
                return Some(label);
            }
            AgentPart::Text { text } | AgentPart::Reasoning { text, .. }
                if item.role == TimelineRole::Assistant =>
            {
                if let Some(line) = text.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                    return Some(derive_task_title(line));
                }
            }
            _ => {}
        }
    }
    None
}

fn collect_assistant_text(timeline: &[TimelineItem]) -> String {
    let mut out = String::new();
    for item in timeline {
        if item.role == TimelineRole::Assistant {
            if let AgentPart::Text { text } = &item.part {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }
    out
}

fn last_assistant_line(timeline: &[TimelineItem]) -> Option<String> {
    for item in timeline.iter().rev() {
        if item.role == TimelineRole::Assistant {
            if let AgentPart::Text { text } = &item.part {
                if let Some(line) = text.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
                    return Some(derive_task_title(line));
                }
            }
        }
    }
    None
}

/// Parses `MUQUN_TASK_DONE:`, `MUQUN_TASK_BLOCKED:`, or `MUQUN_TASK_FAILED:`
/// from assistant text or terminal output, returning `(target_state, summary)`.
pub(crate) fn parse_task_markers(text: &str) -> Option<(&'static str, String)> {
    for raw_line in text.lines().rev() {
        let line = raw_line.trim().trim_matches('`').trim();
        if let Some(rest) = line.strip_prefix("MUQUN_TASK_DONE:") {
            let note = rest.trim();
            return Some((
                "done",
                if note.is_empty() {
                    "Task completed".to_owned()
                } else {
                    note.to_owned()
                },
            ));
        }
        if let Some(rest) = line.strip_prefix("MUQUN_TASK_BLOCKED:") {
            let note = rest.trim();
            return Some((
                "waiting",
                if note.is_empty() {
                    "Waiting for user input".to_owned()
                } else {
                    note.to_owned()
                },
            ));
        }
        if let Some(rest) = line.strip_prefix("MUQUN_TASK_FAILED:") {
            let note = rest.trim();
            return Some((
                "failed",
                if note.is_empty() {
                    "Task failed".to_owned()
                } else {
                    note.to_owned()
                },
            ));
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "muqun-local-hub-test-{label}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn worktree_isolation_creates_branch_and_directory_in_git_repo() {
        let dir = temp_test_dir("worktree");
        let run_git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git command")
        };
        assert!(run_git(&["init"]).status.success());
        assert!(run_git(&["config", "user.email", "test@example.com"])
            .status
            .success());
        assert!(run_git(&["config", "user.name", "Test"]).status.success());
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        assert!(run_git(&["add", "README.md"]).status.success());
        assert!(run_git(&["commit", "-m", "init"]).status.success());

        let (wt_path, branch) = prepare_task_worktree(&dir, "task-1");
        assert_eq!(branch.as_deref(), Some("muqun/task-1"));
        assert_ne!(wt_path, dir);
        assert!(wt_path.join("README.md").is_file());

        // Non-git directory gracefully falls back to base directory
        let non_git = temp_test_dir("non-git");
        let (fallback_path, fallback_branch) = prepare_task_worktree(&non_git, "task-2");
        assert_eq!(fallback_path, non_git);
        assert_eq!(fallback_branch, None);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&non_git);
    }

    #[test]
    fn task_manager_persists_and_transitions_states() {
        let dir = temp_test_dir("manager");
        let store_path = dir.join("local-tasks.json");
        let mgr = LocalTaskManager::with_path(store_path.clone());

        let id1 = mgr.alloc_id();
        assert_eq!(id1, "task-1");

        let row = TaskRow {
            id: id1.clone(),
            state: "working".into(),
            kind: "delegation".into(),
            goal: "Implement feature X".into(),
            title: "Implement feature X".into(),
            origin: "device".into(),
            updated_ms: 100,
            started_ms: 100,
            agent_id: Some("claude".into()),
            asid: Some("ses-1".into()),
            pane_id: Some("task-1".into()),
            worktree_branch: Some("muqun/task-1".into()),
            cwd: Some("/tmp/wt".into()),
            ..TaskRow::default()
        };
        mgr.insert(
            row,
            TaskStep {
                at_ms: 100,
                to: "working".into(),
                note: Some("Delegated".into()),
            },
        );

        // Simulate agent session going Idle without MUQUN_TASK_DONE -> idle_unconfirmed
        let mut snap = AgentSessionSnapshot {
            info: AgentSessionInfo {
                asid: AgentSessionId("ses-1".into()),
                agent_id: "claude".into(),
                backend_session_id: "ses-1".into(),
                runtime: None,
                context_usage: None,
                git_info: None,
                agent_name: None,
                agent_role: None,
                title: "Implement feature X".into(),
                mode: None,
                model: None,
                status: AgentSessionStatus::Idle,
                directory: Some("/tmp/wt".into()),
                cost: None,
                tokens: None,
                limit: None,
                parent_id: None,
                project_id: None,
                outcome: None,
                error: None,
                revert: None,
                fork: None,
                time_idle: None,
                time_viewed: None,
                deleted: false,
                updated_ms: 200,
            },
            timeline: vec![TimelineItem {
                id: "item-1".into(),
                message_id: "msg-1".into(),
                role: TimelineRole::Assistant,
                part: AgentPart::Text {
                    text: "I have updated the module and added tests.".into(),
                },
                seq: 1,
                updated_ms: 200,
                ordinal: 0,
                attachments: None,
                image_assets: None,
                generation: None,
                origin: None,
            }],
            permissions: Vec::new(),
            forms: Vec::new(),
            inbox: Vec::new(),
            seq: 1,
        };
        mgr.reconcile_snapshot(&id1, &snap);

        let paused = mgr.get(&id1).unwrap();
        assert_eq!(paused.state, "idle_unconfirmed");
        assert!(paused.completable);
        assert_eq!(
            paused.summary.as_deref(),
            Some("I have updated the module and added tests.")
        );

        // Confirm done via transition_state (as complete_task does)
        mgr.transition_state(&id1, "done", Some("Confirmed done".into()), None);
        let done = mgr.get(&id1).unwrap();
        assert_eq!(done.state, "done");
        assert!(!done.completable);
        assert!(done.is_ended());

        // Reload from disk and verify persistence + history steps
        let mgr2 = LocalTaskManager::with_path(store_path);
        let listed = mgr2.list(None, None);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "task-1");
        assert_eq!(listed[0].state, "done");
        assert_eq!(listed[0].pane_id.as_deref(), Some("task-1"));
        assert_eq!(listed[0].worktree_branch.as_deref(), Some("muqun/task-1"));
        let steps = mgr2.history("task-1");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].to, "working");
        assert_eq!(steps[1].to, "idle_unconfirmed");
        assert_eq!(steps[2].to, "done");

        // Also test MUQUN_TASK_DONE marker auto-completion and cancel_task state
        let id2 = mgr2.alloc_id();
        assert_eq!(id2, "task-2");
        mgr2.insert(
            TaskRow {
                id: id2.clone(),
                state: "working".into(),
                kind: "delegation".into(),
                goal: "Fix bug".into(),
                title: "Fix bug".into(),
                origin: "device".into(),
                ..TaskRow::default()
            },
            TaskStep {
                at_ms: 300,
                to: "working".into(),
                note: None,
            },
        );
        snap.timeline = vec![TimelineItem {
            id: "item-2".into(),
            message_id: "msg-2".into(),
            role: TimelineRole::Assistant,
            part: AgentPart::Text {
                text: "All done.\nMUQUN_TASK_DONE: Fixed null pointer bug".into(),
            },
            seq: 2,
            updated_ms: 400,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        }];
        mgr2.reconcile_snapshot(&id2, &snap);
        let auto_done = mgr2.get(&id2).unwrap();
        assert_eq!(auto_done.state, "done");
        assert_eq!(auto_done.summary.as_deref(), Some("Fixed null pointer bug"));

        // Verify cancel transition on a third task
        let id3 = mgr2.alloc_id();
        assert_eq!(id3, "task-3");
        mgr2.insert(
            TaskRow {
                id: id3.clone(),
                state: "working".into(),
                kind: "command".into(),
                goal: "!cargo check".into(),
                title: "cargo check".into(),
                origin: "device".into(),
                ..TaskRow::default()
            },
            TaskStep {
                at_ms: 500,
                to: "working".into(),
                note: None,
            },
        );
        mgr2.transition_state(&id3, "cancelled", Some("Cancelled by user".into()), None);
        let cancelled = mgr2.get(&id3).unwrap();
        assert_eq!(cancelled.state, "cancelled");
        assert!(cancelled.is_ended());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
