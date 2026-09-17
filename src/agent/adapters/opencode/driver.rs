use std::sync::Arc;

use crate::agent::domain::{
    AgentCatalog, AgentProject, AgentSessionInfo, ModelRef, PermissionDecision, SessionQuery,
};
use super::client::SessionListFilter;
use crate::agent::ports::engine::{AgentEngineError, AgentEnginePort, EngineFuture, FileDiffItem};
use super::client::OpencodeClient;
use super::discovery::OpencodeEndpoint;
use super::mapper;

pub struct OpencodeDriver {
    client: Arc<OpencodeClient>,
}

impl OpencodeDriver {
    pub fn new(endpoint: OpencodeEndpoint) -> Self {
        Self {
            client: Arc::new(OpencodeClient::new(endpoint)),
        }
    }

    pub fn client(&self) -> &OpencodeClient {
        &self.client
    }
}

impl AgentEnginePort for OpencodeDriver {
    fn kind(&self) -> &'static str {
        "opencode"
    }

    fn probe(&self) -> EngineFuture<'_, bool> {
        Box::pin(async move {
            let req_client = reqwest::Client::new();
            Ok(self.client.endpoint.probe_healthy(&req_client).await)
        })
    }

    fn list_projects(&self) -> EngineFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let raw_projects = self.client.list_projects().await?;
            let projects = raw_projects.iter().filter_map(mapper::map_project).collect();
            Ok(projects)
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> EngineFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let filter = SessionListFilter {
                parent_id: query.parent_id.as_deref(),
                limit: query.limit,
                order: query.order.as_deref(),
                search: query.search.as_deref(),
                cursor: query.cursor.as_deref(),
            };
            let raw_sessions = self
                .client
                .list_sessions(query.directory.as_deref(), &filter)
                .await?;
            let sessions = raw_sessions.iter().filter_map(mapper::map_session).collect();
            Ok(sessions)
        })
    }

    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        agent: Option<&'a str>,
    ) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let raw = self.client.create_session(directory, model, agent).await?;
            mapper::map_session(&raw)
                .ok_or_else(|| AgentEngineError::Protocol("Failed to parse created session".to_string()))
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let raw = self.client.get_session(session_id).await?;
            mapper::map_session(&raw)
                .ok_or_else(|| AgentEngineError::SessionNotFound(session_id.to_string()))
        })
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.send_prompt(session_id, text, attachments, delivery).await?;
            Ok(())
        })
    }

    fn revert_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.revert_session(session_id, message_id).await?;
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.interrupt(session_id).await?;
            Ok(())
        })
    }

    fn switch_model<'a>(
        &'a self,
        session_id: &'a str,
        model: &'a ModelRef,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.switch_model(session_id, model).await?;
            Ok(())
        })
    }

    fn switch_agent<'a>(
        &'a self,
        session_id: &'a str,
        agent: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.switch_agent(session_id, agent).await?;
            Ok(())
        })
    }

    fn find_files<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<serde_json::Value>> {
        Box::pin(async move {
            // A failed search is an error, not an empty result set: the two
            // are indistinguishable to the caller otherwise.
            self.client.find_files(query, limit, directory).await
        })
    }

    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            let reply_str = match decision {
                PermissionDecision::Allow => "once",
                PermissionDecision::AllowAlways => "always",
                PermissionDecision::Deny => "reject",
            };
            self.client
                .reply_permission(session_id, request_id, reply_str, message)
                .await?;
            Ok(())
        })
    }

    fn reply_form<'a>(
        &'a self,
        session_id: &'a str,
        form_id: &'a str,
        answers: serde_json::Value,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.reply_form(session_id, form_id, &answers).await?;
            Ok(())
        })
    }

    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> EngineFuture<'a, AgentCatalog> {
        Box::pin(async move {
            // One failing fan-out arm must not empty the whole catalog, but it
            // is worth saying which one failed.
            let log = |what: &str, err: &AgentEngineError| {
                tracing::warn!(surface = what, %err, "catalog fan-out arm failed");
            };
            let raw_models = self
                .client
                .get_models(directory)
                .await
                .inspect_err(|e| log("model", e))
                .unwrap_or_default();
            let raw_agents = self
                .client
                .get_agents(directory)
                .await
                .inspect_err(|e| log("agent", e))
                .unwrap_or_default();
            let raw_mcp = self
                .client
                .get_mcp(directory)
                .await
                .inspect_err(|e| log("mcp", e))
                .unwrap_or_default();
            let raw_skills = self
                .client
                .get_skills(directory)
                .await
                .inspect_err(|e| log("skill", e))
                .unwrap_or_default();
            let raw_providers = self
                .client
                .get_providers(directory)
                .await
                .inspect_err(|e| log("provider", e))
                .unwrap_or_default();
            let raw_commands = self
                .client
                .get_commands(directory)
                .await
                .inspect_err(|e| log("command", e))
                .unwrap_or_default();
            let default_model = self
                .client
                .get_default_model(directory)
                .await
                .inspect_err(|e| log("model/default", e))
                .unwrap_or_default();
            let config = self
                .client
                .get_config(directory)
                .await
                .inspect_err(|e| log("config", e))
                .unwrap_or_default();

            let models = mapper::map_models(&raw_models);
            Ok(AgentCatalog {
                agents: mapper::map_agents(&raw_agents),
                mcp: mapper::map_mcp(&raw_mcp),
                skills: mapper::map_skills(&raw_skills),
                providers: mapper::map_providers(&raw_providers, &models),
                commands: mapper::map_commands(&raw_commands),
                defaults: mapper::map_catalog_defaults(default_model.as_ref(), &config),
                models,
            })
        })
    }

    fn get_vcs_diff<'a>(
        &'a self,
        session_id: &'a str,
        mode: &'a str,
    ) -> EngineFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move {
            // The diff is scoped to the session's own directory; a global diff
            // is not what the caller asked for.
            let directory = match self.client.get_session(session_id).await {
                Ok(raw) => mapper::map_session(&raw).and_then(|s| s.directory),
                Err(err) => {
                    tracing::debug!(session_id, %err, "vcs diff: session lookup failed, using server cwd");
                    None
                }
            };
            let raw_diff = self
                .client
                .get_vcs_diff(directory.as_deref(), mode, None)
                .await?;
            let mut diffs = Vec::new();
            for item in raw_diff {
                // `FileDiff.Info` names the path `file`; `path` is the v1 name.
                let path = item
                    .get("file")
                    .or_else(|| item.get("path"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let patch = item.get("patch").or_else(|| item.get("diff")).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let additions = item.get("additions").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                let deletions = item.get("deletions").and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
                diffs.push(FileDiffItem {
                    path,
                    patch,
                    additions,
                    deletions,
                });
            }
            Ok(diffs)
        })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::PermissionRequest>> {
        Box::pin(async move {
            let raw = self.client.get_session_permissions(session_id).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(raw
                .iter()
                .filter_map(|p| mapper::map_permission_request(p, &asid))
                .collect())
        })
    }

    fn get_pending_forms<'a>(
        &'a self,
        session_id: &'a str,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::FormRequest>> {
        Box::pin(async move {
            let raw = self.client.get_session_forms(session_id).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(raw
                .iter()
                .filter_map(|f| mapper::map_form_request(f, &asid))
                .collect())
        })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<crate::agent::domain::TimelineItem>> {
        Box::pin(async move {
            let messages = self.client.get_messages(session_id, limit).await?;
            let asid = crate::agent::domain::AgentSessionId(session_id.to_string());
            Ok(mapper::map_messages_to_timeline(&messages, &asid))
        })
    }
}
