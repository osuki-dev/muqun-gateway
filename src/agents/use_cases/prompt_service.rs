use std::sync::Arc;

use crate::agents::domain::AgentSessionId;
use crate::agents::ports::agent::{AgentError, AgentPort};
use crate::agents::ports::mirror::SessionMirrorPort;

pub struct PromptService {
    agent: Arc<dyn AgentPort>,
    _mirror: Arc<dyn SessionMirrorPort>,
}

impl PromptService {
    pub fn new(agent: Arc<dyn AgentPort>, _mirror: Arc<dyn SessionMirrorPort>) -> Self {
        Self { agent, _mirror }
    }

    pub async fn send_prompt(
        &self,
        asid: &AgentSessionId,
        text: &str,
        attachments: &[String],
        delivery: Option<&str>,
    ) -> Result<(), AgentError> {
        self.agent
            .send_prompt(&asid.0, text, attachments, delivery)
            .await
    }

    pub async fn interrupt(&self, asid: &AgentSessionId) -> Result<(), AgentError> {
        self.agent.interrupt(&asid.0).await
    }
}
