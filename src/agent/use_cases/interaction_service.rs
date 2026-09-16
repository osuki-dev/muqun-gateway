use std::sync::Arc;

use crate::agent::domain::{AgentSessionId, PermissionDecision};
use crate::agent::ports::engine::{AgentEngineError, AgentEnginePort};
use crate::agent::ports::mirror::SessionMirrorPort;

pub struct InteractionService {
    engine: Arc<dyn AgentEnginePort>,
    mirror: Arc<dyn SessionMirrorPort>,
}

impl InteractionService {
    pub fn new(engine: Arc<dyn AgentEnginePort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { engine, mirror }
    }

    pub async fn reply_permission(
        &self,
        asid: &AgentSessionId,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AgentEngineError> {
        self.engine.reply_permission(&asid.0, request_id, decision).await?;
        self.mirror.resolve_permission(asid, request_id).await;
        Ok(())
    }

    pub async fn reply_form(
        &self,
        asid: &AgentSessionId,
        form_id: &str,
        answers: serde_json::Value,
    ) -> Result<(), AgentEngineError> {
        self.engine.reply_form(&asid.0, form_id, answers).await?;
        self.mirror.resolve_form(asid, form_id).await;
        Ok(())
    }
}
