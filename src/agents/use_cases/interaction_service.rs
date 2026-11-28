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
        // Another device already cleared it: succeed quietly (G3 R4).
        if self.permission_cleared(asid, request_id).await {
            return Ok(());
        }
        match self
            .agent
            .reply_permission(&asid.0, request_id, decision, message)
            .await
        {
            Ok(()) => {
                self.mirror.resolve_permission(asid, request_id).await;
                Ok(())
            }
            Err(err) => {
                if self.permission_cleared(asid, request_id).await
                    || already_resolved_permission(&err)
                {
                    self.mirror.resolve_permission(asid, request_id).await;
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub async fn reply_form(
        &self,
        asid: &AgentSessionId,
        form_id: &str,
        answers: serde_json::Value,
    ) -> Result<(), AgentError> {
        if self.form_cleared(asid, form_id).await {
            return Ok(());
        }
        match self.agent.reply_form(&asid.0, form_id, answers).await {
            Ok(()) => {
                self.mirror.resolve_form(asid, form_id).await;
                Ok(())
            }
            Err(err) => {
                if self.form_cleared(asid, form_id).await || already_resolved_form(&err) {
                    self.mirror.resolve_form(asid, form_id).await;
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn permission_cleared(&self, asid: &AgentSessionId, request_id: &str) -> bool {
        match self.mirror.get_snapshot(asid).await {
            Some(snap) => !snap.permissions.iter().any(|p| p.id == request_id),
            None => false,
        }
    }

    async fn form_cleared(&self, asid: &AgentSessionId, form_id: &str) -> bool {
        match self.mirror.get_snapshot(asid).await {
            Some(snap) => !snap.forms.iter().any(|f| f.id == form_id),
            None => false,
        }
    }
}

fn already_resolved_permission(err: &AgentError) -> bool {
    match err {
        AgentError::InvalidRequest(msg) => {
            let lower = msg.to_ascii_lowercase();
            lower.contains("no longer pending")
                || lower.contains("not pending")
                || lower.contains("unknown permission")
                || lower.contains("already resolved")
        }
        _ => false,
    }
}

fn already_resolved_form(err: &AgentError) -> bool {
    match err {
        AgentError::InvalidRequest(msg) => {
            let lower = msg.to_ascii_lowercase();
            lower.contains("no longer pending")
                || lower.contains("not pending")
                || lower.contains("unknown form")
                || lower.contains("already resolved")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::memory_mirror::MemoryMirror;
    use crate::agents::domain::{FormRequest, PermissionRequest};
    use crate::test_support::{fake_session, FakeAgent};

    fn permission(asid: &str, id: &str) -> PermissionRequest {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "asid": asid,
            "action": "bash",
            "resources": ["ls"],
            "prompt": "run",
            "options": [],
        }))
        .unwrap()
    }

    fn form(asid: &str, id: &str) -> FormRequest {
        FormRequest {
            nonblocking: false,
            id: id.into(),
            asid: AgentSessionId(asid.into()),
            title: "ask".into(),
            fields: vec![],
        }
    }

    #[tokio::test]
    async fn a_duplicate_permission_reply_succeeds_quietly() {
        let mirror = Arc::new(MemoryMirror::new());
        let asid = AgentSessionId("s1".into());
        mirror.update_session(fake_session("s1", 1)).await;
        mirror.add_permission(permission("s1", "p1")).await;

        let agent = Arc::new(FakeAgent::new("claude"));
        let service = InteractionService::new(agent.clone(), mirror.clone());

        service
            .reply_permission(&asid, "p1", PermissionDecision::Allow, None)
            .await
            .unwrap();
        assert!(mirror
            .get_snapshot(&asid)
            .await
            .unwrap()
            .permissions
            .is_empty());

        // Second client: mirror already clear — no agent call, no error.
        let before = agent.calls.lock().unwrap().len();
        service
            .reply_permission(&asid, "p1", PermissionDecision::Deny, None)
            .await
            .unwrap();
        assert_eq!(agent.calls.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn an_agent_that_says_permission_gone_still_clears_the_mirror() {
        let mirror = Arc::new(MemoryMirror::new());
        let asid = AgentSessionId("s2".into());
        mirror.update_session(fake_session("s2", 1)).await;
        mirror.add_permission(permission("s2", "p2")).await;

        let mut agent = FakeAgent::new("claude");
        agent.reply_permission_gone = true;
        let agent = Arc::new(agent);
        let service = InteractionService::new(agent, mirror.clone());

        service
            .reply_permission(&asid, "p2", PermissionDecision::Allow, None)
            .await
            .unwrap();
        assert!(mirror
            .get_snapshot(&asid)
            .await
            .unwrap()
            .permissions
            .is_empty());
    }

    #[tokio::test]
    async fn a_duplicate_form_reply_succeeds_quietly() {
        let mirror = Arc::new(MemoryMirror::new());
        let asid = AgentSessionId("s3".into());
        mirror.update_session(fake_session("s3", 1)).await;
        mirror.add_form(form("s3", "f1")).await;

        let agent = Arc::new(FakeAgent::new("claude"));
        let service = InteractionService::new(agent, mirror.clone());

        service
            .reply_form(&asid, "f1", serde_json::json!({}))
            .await
            .unwrap();
        service
            .reply_form(&asid, "f1", serde_json::json!({}))
            .await
            .unwrap();
        assert!(mirror.get_snapshot(&asid).await.unwrap().forms.is_empty());
    }
}
