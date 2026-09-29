use std::sync::Arc;

use crate::agents::domain::{AgentSessionId, PermissionDecision};
use crate::agents::ports::agent::{AgentError, AgentPort};
use crate::agents::ports::mirror::SessionMirrorPort;

pub struct InteractionService {
    agent: Arc<dyn AgentPort>,
    mirror: Arc<dyn SessionMirrorPort>,
}

impl InteractionService {
    pub fn new(agent: Arc<dyn AgentPort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { agent, mirror }
    }

    pub async fn reply_permission(
        &self,
        asid: &AgentSessionId,
        request_id: &str,
        decision: PermissionDecision,
        message: Option<&str>,
    ) -> Result<(), AgentError> {
        self.agent
            .reply_permission(&asid.0, request_id, decision, message)
            .await?;
        self.mirror.resolve_permission(asid, request_id).await;
        Ok(())
    }

    pub async fn reply_form(
        &self,
        asid: &AgentSessionId,
        form_id: &str,
        answers: serde_json::Value,
    ) -> Result<(), AgentError> {
        self.agent.reply_form(&asid.0, form_id, answers).await?;
        self.mirror.resolve_form(asid, form_id).await;
        Ok(())
    }
}
