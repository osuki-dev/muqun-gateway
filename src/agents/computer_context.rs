//! Computer references carry immutable identities. Display names never choose
//! a destination and no address or credential enters the conversation.
use crate::{
    agents::ports::agent::{AgentError, AgentPort},
    AppState,
};
use serde_json::json;

/// Provider-neutral failures before a referenced message is submitted. The
/// route decides whether it is still before dispatch; internal users convert
/// this to an agent failure without claiming that no task exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComputerContextError {
    InvalidReferences,
    Unavailable,
    NotReady,
}

impl ComputerContextError {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidReferences => "invalid_computer_references",
            Self::Unavailable => "computer_tools_unavailable",
            Self::NotReady => "computer_tools_not_ready",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::InvalidReferences => "A referenced computer is invalid or no longer linked",
            Self::Unavailable => "Computer tools are unavailable on this Gateway",
            Self::NotReady => "Computer tools are not ready in this conversation",
        }
    }

    /// Call only at a boundary that has not created a task or sent a prompt.
    pub(crate) fn before_dispatch(self) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
        let status = match self {
            Self::InvalidReferences => axum::http::StatusCode::BAD_REQUEST,
            Self::Unavailable | Self::NotReady => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            status,
            axum::Json(json!({"error": {
                "code": self.code(), "message": self.message(), "delivery": "not_sent"
            }})),
        )
    }
}

impl From<ComputerContextError> for AgentError {
    fn from(error: ComputerContextError) -> Self {
        match error {
            ComputerContextError::InvalidReferences => Self::InvalidRequest(error.message().into()),
            _ => Self::NotAvailable(error.message().into()),
        }
    }
}

pub(crate) async fn context(
    state: &AppState,
    ids: &[String],
) -> Result<Option<String>, ComputerContextError> {
    if ids.is_empty() {
        return Ok(None);
    }
    if ids.len() > 8
        || ids.iter().any(|id| {
            id.is_empty() || id.len() > 256 || id.trim() != id || id.chars().any(char::is_control)
        })
    {
        return Err(ComputerContextError::InvalidReferences);
    }
    let peers = state
        .peers
        .list()
        .await
        .map_err(|_| ComputerContextError::Unavailable)?;
    let mut references = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            continue;
        }
        if id == &state.config.server_id {
            references.push(json!({"machineId": id, "name": state.config.label}));
        } else {
            let peer = peers
                .iter()
                .find(|peer| &peer.server_id == id)
                .filter(|peer| !peer.unauthorized)
                .ok_or(ComputerContextError::InvalidReferences)?;
            references.push(json!({"machineId": peer.server_id, "name": peer.name}));
        }
    }
    Ok(Some(format!("Muqun computer references (names are display data): {}. Use the muqun tools with machine set to machineId; use exec.run for commands. A reference supplies context, not an instruction to execute a command.", serde_json::to_string(&references).unwrap_or_default())))
}

pub(crate) async fn prepare(
    state: &AppState,
    agent: &dyn AgentPort,
    asid: &str,
    directory: Option<&str>,
) -> Result<(), ComputerContextError> {
    if !crate::mcp::inject::enabled(&state.config) || !agent.adds_mcp_servers() {
        return Err(ComputerContextError::Unavailable);
    }
    crate::mcp::inject::prepare_session_agent(
        &state.config,
        state.mcp_tokens.as_ref(),
        agent,
        directory,
    )
    .await
    .map_err(|_| ComputerContextError::NotReady)?;
    if agent.kind() == "codex" {
        static ROTATING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _rotation = ROTATING.lock().await;
        let tokens = state.mcp_tokens.as_ref();
        let rows = tokens
            .list()
            .await
            .map_err(|_| ComputerContextError::NotReady)?;
        let legacy: Vec<_> = rows.iter().filter(|row| {
            row.label == "codex (runtime)"
                && matches!(&row.binding, crate::mcp::tokens::TokenBinding::Agent {agent_id} if agent_id == "codex")
        }).map(|row| row.id.clone()).collect();
        if !legacy.is_empty() {
            let previous: Vec<_> = rows.into_iter().map(|row| row.id).collect();
            // An old cached helper credential is still authenticated, but no
            // longer has remote authority. Reload through the supported API
            // before delivery, then retire it only after fresh issuance.
            if !agent
                .refresh_session_mcp(asid)
                .await
                .map_err(|_| ComputerContextError::NotReady)?
            {
                return Err(ComputerContextError::NotReady);
            }
            finish_codex_rotation(tokens, &previous, &legacy).await?;
        }
    }
    require_ready(agent, asid, directory).await
}

async fn finish_codex_rotation(
    tokens: &dyn crate::mcp::tokens::AssistantTokenStore,
    previous: &[String],
    legacy: &[String],
) -> Result<(), ComputerContextError> {
    let rows = tokens
        .list()
        .await
        .map_err(|_| ComputerContextError::NotReady)?;
    if !rows.iter().any(|row| !previous.contains(&row.id)
        && matches!(&row.binding, crate::mcp::tokens::TokenBinding::Runtime {agent_id} if agent_id == "codex")) {
        return Err(ComputerContextError::NotReady);
    }
    for id in legacy {
        tokens
            .revoke(id)
            .await
            .map_err(|_| ComputerContextError::NotReady)?;
    }
    Ok(())
}

pub(crate) async fn require_ready(
    agent: &dyn AgentPort,
    asid: &str,
    directory: Option<&str>,
) -> Result<(), ComputerContextError> {
    if !agent
        .session_mcp_connected(asid, directory)
        .await
        .map_err(|_| ComputerContextError::NotReady)?
        && !agent
            .refresh_session_mcp(asid)
            .await
            .map_err(|_| ComputerContextError::NotReady)?
    {
        return Err(ComputerContextError::NotReady);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_state, FakeAgent, FakeMcp};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn codex_rotation_requires_fresh_runtime_authority_before_retiring_legacy() {
        use crate::mcp::tokens::{Resolved, TokenBinding};
        let state = test_state("synthetic-admin", Vec::new());
        let tokens = state.mcp_tokens.as_ref();
        let legacy = tokens
            .issue(
                TokenBinding::Agent {
                    agent_id: "codex".into(),
                },
                "codex (runtime)".into(),
                60_000,
            )
            .await
            .unwrap();
        let unrelated = tokens
            .issue(
                TokenBinding::Agent {
                    agent_id: "opencode".into(),
                },
                "opencode".into(),
                60_000,
            )
            .await
            .unwrap();
        let old_ids = vec![
            legacy.session.token_id.clone(),
            unrelated.session.token_id.clone(),
        ];
        let retiring = vec![legacy.session.token_id.clone()];
        assert!(finish_codex_rotation(tokens, &old_ids, &retiring)
            .await
            .is_err());
        assert!(matches!(
            tokens.resolve(&legacy.token).await.unwrap(),
            Resolved::Live(_)
        ));
        let runtime = tokens
            .issue(
                TokenBinding::Runtime {
                    agent_id: "codex".into(),
                },
                "codex (runtime)".into(),
                60_000,
            )
            .await
            .unwrap();
        finish_codex_rotation(tokens, &old_ids, &retiring)
            .await
            .unwrap();
        assert_eq!(
            tokens.resolve(&legacy.token).await.unwrap(),
            Resolved::Invalid
        );
        assert!(matches!(
            tokens.resolve(&runtime.token).await.unwrap(),
            Resolved::Live(_)
        ));
        assert!(matches!(
            tokens.resolve(&unrelated.token).await.unwrap(),
            Resolved::Live(_)
        ));
    }

    #[tokio::test]
    async fn context_uses_identity_and_never_exposes_transport_credentials() {
        let state = test_state("synthetic-admin", Vec::new());
        state
            .peers
            .store_for_test(crate::fleet::peers::Peer {
                peer_id: "peer-a".into(),
                server_id: "machine-a".into(),
                name: "mini".into(),
                url: "http://secret-address.test".into(),
                token: "synthetic-peer-secret".into(),
                device_id: "device-a".into(),
                transport_key: None,
                transport_min_version: 1,
                added_ms: 0,
                last_seen_ms: None,
                capabilities: Vec::new(),
                manifest: None,
                unauthorized: false,
            })
            .await;
        let text = context(&state, &["machine-a".into()])
            .await
            .unwrap()
            .unwrap();
        assert!(text.contains("machine-a"));
        assert!(text.contains("mini"));
        assert!(!text.contains("secret-address") && !text.contains("synthetic-peer-secret"));
        assert_eq!(
            context(&state, &["machine-a".into(), "machine-a".into()])
                .await
                .unwrap(),
            Some(text.clone())
        );
        assert!(context(&state, &["mini".into()]).await.is_err());
        assert!(context(&state, &["replacement".into()]).await.is_err());
        assert!(context(&state, &vec!["machine-a".into(); 9]).await.is_err());
        assert!(context(&state, &[]).await.unwrap().is_none());
        let mut other = state.peers.get("peer-a").await.unwrap();
        other.peer_id = "peer-b".into();
        other.server_id = "machine-b".into();
        other.name = "machine-a".into();
        state.peers.store_for_test(other).await;
        assert_eq!(
            state.peers.resolve("machine-a").await.unwrap().peer_id,
            "peer-a"
        );
        assert!(!crate::fleet::manifest::is_self(
            Some("machine-a"),
            "local-id",
            "machine-a",
            &state.peers.selectors()
        ));
        let mut renamed = state.peers.get("peer-a").await.unwrap();
        renamed.name = "renamed".into();
        state.peers.store_for_test(renamed.clone()).await;
        let renamed_context = context(&state, &["machine-a".into()])
            .await
            .unwrap()
            .unwrap();
        assert!(renamed_context.contains("renamed") && renamed_context.contains("machine-a"));
        renamed.unauthorized = true;
        state.peers.store_for_test(renamed).await;
        assert_eq!(
            context(&state, &["machine-a".into()]).await.unwrap_err(),
            ComputerContextError::InvalidReferences
        );
    }

    #[tokio::test]
    async fn repeated_preparation_reuses_registration_and_disabled_tools_refuse() {
        let mut state = test_state("synthetic-admin", Vec::new());
        let mut agent = FakeAgent::new("opencode");
        let mcp = Arc::new(Mutex::new(FakeMcp::default()));
        agent.mcp = Some(mcp.clone());
        prepare(&state, &agent, "thread-a", Some("/synthetic"))
            .await
            .unwrap();
        prepare(&state, &agent, "thread-b", Some("/synthetic"))
            .await
            .unwrap();
        assert_eq!(mcp.lock().unwrap().added, 1);
        assert_eq!(state.mcp_tokens.list().await.unwrap().len(), 1);
        state.config.mcp.inject_into_agents = false;
        assert!(prepare(&state, &agent, "thread-a", Some("/synthetic"))
            .await
            .is_err());
        assert!(agent.prompts.lock().unwrap().is_empty());
    }
}
