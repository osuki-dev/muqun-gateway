use serde_json::Value;
use std::sync::Arc;

use super::client::DeepseekClient;
use super::endpoint::DeepseekEndpoint;
use super::mapper;
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, FormRequest,
    ModelRef, PermissionDecision, PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::engine::{AgentEnginePort, EngineFuture, FileDiffItem};

pub struct DeepseekDriver {
    pub client: Arc<DeepseekClient>,
}

impl DeepseekDriver {
    pub fn new(endpoint: DeepseekEndpoint) -> Self {
        Self {
            client: Arc::new(DeepseekClient::new(endpoint)),
        }
    }

    pub fn client(&self) -> &DeepseekClient {
        &self.client
    }
}

impl AgentEnginePort for DeepseekDriver {
    fn kind(&self) -> &'static str {
        "deepseek"
    }

    fn probe(&self) -> EngineFuture<'_, bool> {
        Box::pin(async move {
            let req_client = reqwest::Client::new();
            Ok(self.client.endpoint.probe_healthy(&req_client).await)
        })
    }

    fn list_projects(&self) -> EngineFuture<'_, Vec<AgentProject>> {
        Box::pin(async move {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string());

            let name = std::path::Path::new(&cwd)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "workspace".to_string());

            Ok(vec![AgentProject {
                id: "default".to_string(),
                canonical: cwd,
                name,
                vcs: Some("git".to_string()),
                sandboxes: vec![],
                missing: false,
            }])
        })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> EngineFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            let raw_sessions = self.client.list_sessions(query).await?;
            let sessions: Vec<AgentSessionInfo> = raw_sessions
                .iter()
                .filter_map(mapper::map_session)
                .collect();
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
            let res = self.client.create_session(directory, model, agent).await?;
            let session_id = res
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("ses_created");

            if let Some(m) = model {
                let _ = self
                    .client
                    .select_model(
                        session_id,
                        &m.provider_id,
                        &m.model_id,
                        m.variant.as_deref(),
                    )
                    .await;
            }

            Ok(AgentSessionInfo {
                asid: AgentSessionId(session_id.to_string()),
                backend_session_id: session_id.to_string(),
                title: "New DeepSeek Session".to_string(),
                agent: agent.map(str::to_string),
                model: model.cloned(),
                status: AgentSessionStatus::Idle,
                directory: directory.map(str::to_string),
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
                updated_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            })
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            // First try get_projections which gives complete session projection
            if let Ok(proj) = self.client.get_projections(session_id).await {
                let wrapped = serde_json::json!({
                    "sessionId": session_id,
                    "projections": proj,
                });
                if let Some(mut info) = mapper::map_session(&wrapped) {
                    info.asid = AgentSessionId(session_id.to_string());
                    return Ok(info);
                }
            }

            let page = self
                .client
                .get_page(session_id, 1)
                .await
                .unwrap_or(Value::Null);
            let mut info = mapper::map_session(&page).unwrap_or_else(|| AgentSessionInfo {
                asid: AgentSessionId(session_id.to_string()),
                backend_session_id: session_id.to_string(),
                title: "DeepSeek Session".to_string(),
                agent: None,
                model: None,
                status: AgentSessionStatus::Idle,
                directory: None,
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
                updated_ms: 0,
            });
            info.asid = AgentSessionId(session_id.to_string());
            Ok(info)
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
            self.client
                .send_prompt(session_id, text, attachments, delivery)
                .await?;
            Ok(())
        })
    }

    fn revert_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            // DeepSeek Harness uses fork rather than destructive rollback
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.cancel(session_id).await?;
            Ok(())
        })
    }

    fn switch_model<'a>(
        &'a self,
        session_id: &'a str,
        model: &'a ModelRef,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client
                .select_model(
                    session_id,
                    &model.provider_id,
                    &model.model_id,
                    model.variant.as_deref(),
                )
                .await?;
            Ok(())
        })
    }

    fn switch_agent<'a>(&'a self, _session_id: &'a str, _agent: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        directory: Option<&'a str>,
    ) -> EngineFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let cwd = directory.unwrap_or(".");
            match self.client.workspace_files_list(cwd, "").await {
                Ok(res) => {
                    let files = res
                        .get("files")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    Ok(files)
                }
                Err(_) => Ok(vec![]),
            }
        })
    }

    fn reply_permission<'a>(
        &'a self,
        _session_id: &'a str,
        _request_id: &'a str,
        _decision: PermissionDecision,
        _message: Option<&'a str>,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn reply_form<'a>(
        &'a self,
        _session_id: &'a str,
        _form_id: &'a str,
        _answers: Value,
    ) -> EngineFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> EngineFuture<'a, AgentCatalog> {
        Box::pin(async move {
            match self.client.model_catalog().await {
                Ok(raw) => Ok(mapper::map_catalog(&raw)),
                Err(_) => Ok(mapper::map_catalog(&Value::Null)),
            }
        })
    }

    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> EngineFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async move { Ok(vec![]) })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> EngineFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move { Ok(vec![]) })
    }

    fn get_pending_forms<'a>(&'a self, _session_id: &'a str) -> EngineFuture<'a, Vec<FormRequest>> {
        Box::pin(async move { Ok(vec![]) })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> EngineFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let res = self
                .client
                .get_page(session_id, limit)
                .await
                .unwrap_or(Value::Null);
            Ok(mapper::map_timeline_records(&res))
        })
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.cancel(session_id).await?;
            Ok(())
        })
    }

    fn rename_session<'a>(&'a self, session_id: &'a str, title: &'a str) -> EngineFuture<'a, ()> {
        Box::pin(async move {
            self.client.rename(session_id, title).await?;
            Ok(())
        })
    }

    fn fork_session<'a>(
        &'a self,
        session_id: &'a str,
        _message_id: Option<&'a str>,
    ) -> EngineFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let res = self.client.fork(session_id).await?;
            let new_id = res
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("ses_forked");

            Ok(AgentSessionInfo {
                asid: AgentSessionId(new_id.to_string()),
                backend_session_id: new_id.to_string(),
                title: "Forked Session".to_string(),
                agent: None,
                model: None,
                status: AgentSessionStatus::Idle,
                directory: None,
                cost: None,
                tokens: None,
                limit: None,
                parent_id: Some(session_id.to_string()),
                project_id: None,
                outcome: None,
                error: None,
                revert: None,
                fork: None,
                time_idle: None,
                time_viewed: None,
                deleted: false,
                updated_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_live_deepseek_driver_session_lifecycle() {
        let Some(endpoint) = DeepseekEndpoint::discover().await else {
            eprintln!("DeepSeek Harness not discovered, skipping live test");
            return;
        };

        let driver = DeepseekDriver::new(endpoint);
        let catalog = driver
            .get_catalog(None)
            .await
            .expect("Catalog fetch failed");
        assert!(!catalog.models.is_empty());
        println!(
            "Live Catalog Models: {:?}",
            catalog.models.iter().map(|m| &m.name).collect::<Vec<_>>()
        );

        let temp_dir = "/tmp/gateway_deepseek_live_test";
        let _ = std::fs::create_dir_all(temp_dir);

        let session = driver
            .create_session(Some(temp_dir), None, None)
            .await
            .expect("Session creation failed");
        let session_id = session.asid.0.clone();
        println!("Created Live DeepSeek Session: {session_id}");

        let fetched = driver
            .get_session(&session_id)
            .await
            .expect("Fetch session failed");
        assert_eq!(fetched.asid.0, session_id);
        println!("Fetched Session Info: {:?}", fetched);

        let prompt_res = driver
            .send_prompt(
                &session_id,
                "Ping from Rust DeepseekDriver live test!",
                &[],
                None,
            )
            .await;
        assert!(prompt_res.is_ok(), "Prompt send failed: {:?}", prompt_res);
        println!("Successfully sent prompt to live DeepSeek session!");

        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
        let timeline = driver
            .get_timeline(&session_id, 10)
            .await
            .expect("Get timeline failed");
        println!("Timeline items count: {}", timeline.len());
        for item in &timeline {
            println!("  [Role {:?}] Part: {:?}", item.role, item.part);
        }

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
