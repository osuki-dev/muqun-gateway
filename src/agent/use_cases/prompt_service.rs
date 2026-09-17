use std::sync::Arc;

use crate::agent::domain::AgentSessionId;
use crate::agent::ports::engine::{AgentEngineError, AgentEnginePort};
use crate::agent::ports::mirror::SessionMirrorPort;

pub struct PromptService {
    engine: Arc<dyn AgentEnginePort>,
    mirror: Arc<dyn SessionMirrorPort>,
}

impl PromptService {
    pub fn new(engine: Arc<dyn AgentEnginePort>, mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { engine, mirror }
    }

    pub async fn send_prompt(
        &self,
        asid: &AgentSessionId,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<(), AgentEngineError> {
        self.engine.send_prompt(&asid.0, text, attachments, delivery).await
    }

    pub async fn interrupt(&self, asid: &AgentSessionId) -> Result<(), AgentEngineError> {
        self.engine.interrupt(&asid.0).await
    }
}
