//! Correlate a provider conversation with current local terminal observations.
//! No process discovery, history inspection, title/cwd heuristics, or terminal input.
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use tokio::sync::Mutex;
use tokio::time::Instant;

use super::domain::*;
use crate::backend::{Agent, AgentStatus, BackendKind, TerminalBackend};

const CACHE_TTL: Duration = Duration::from_millis(250);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BACKENDS: usize = 32;
const MAX_AGENTS: usize = 4096;

#[derive(Default, Clone)]
struct Observations {
    agents: Vec<(String, Agent)>,
    incomplete: bool,
}

pub(crate) struct TerminalRuntime {
    backends: Vec<(String, Arc<dyn TerminalBackend>)>,
    overflow: bool,
    cache: Mutex<Option<(Instant, Observations)>>,
}

impl TerminalRuntime {
    pub(crate) fn new(sessions: &[crate::SessionConfig]) -> Arc<Self> {
        // Only Herdr currently supplies an authoritative conversation identity.
        // The same configured endpoint must not become two competing bindings.
        let mut endpoints = std::collections::HashSet::new();
        let sessions: Vec<_> = sessions
            .iter()
            .filter(|s| s.backend == BackendKind::Herdr)
            .filter(|s| endpoints.insert(crate::backend_endpoint(s)))
            .collect();
        Arc::new(Self {
            overflow: sessions.len() > MAX_BACKENDS,
            backends: sessions
                .into_iter()
                .take(MAX_BACKENDS)
                .map(|s| (s.id.clone(), Arc::from(crate::terminal_backend(s))))
                .collect(),
            cache: Mutex::new(None),
        })
    }

    async fn observations(&self, fresh: bool) -> Observations {
        // Coalesce concurrent view reads. Mutations bypass a cached observation.
        let mut cache = self.cache.lock().await;
        if !fresh {
            if let Some((_, observation)) =
                cache.as_ref().filter(|(at, _)| at.elapsed() < CACHE_TTL)
            {
                return observation.clone();
            }
        }
        let results = join_all(self.backends.iter().map(|(id, backend)| async move {
            (
                id,
                tokio::time::timeout(READ_TIMEOUT, backend.list_agents()).await,
            )
        }))
        .await;
        let mut observations = Observations {
            incomplete: self.overflow,
            ..Default::default()
        };
        for (id, result) in results {
            match result {
                Ok(Ok(agents)) if observations.agents.len() + agents.len() <= MAX_AGENTS => {
                    observations
                        .agents
                        .extend(agents.into_iter().map(|agent| (id.clone(), agent)));
                }
                _ => observations.incomplete = true,
            }
        }
        *cache = Some((Instant::now(), observations.clone()));
        observations
    }

    pub(crate) async fn observe(&self, id: &str, native: bool, fresh: bool) -> SessionRuntime {
        resolve(&self.observations(fresh).await, id, native)
    }
}

fn unavailable(reason: RuntimeReason) -> SessionRuntime {
    SessionRuntime {
        source: RuntimeSource::Unobserved,
        interaction: RuntimeInteraction::Unavailable,
        status: None,
        terminal: None,
        reason: Some(reason),
    }
}

fn resolve(observations: &Observations, id: &str, native: bool) -> SessionRuntime {
    // UUID parsing deliberately does not authorize a fuzzy/case-normalized join.
    // Both native strings must compare exactly as supplied by their backends.
    let matches: Vec<_> = observations
        .agents
        .iter()
        .filter(|(_, agent)| {
            agent.kind.as_deref() == Some("codex")
                && uuid::Uuid::parse_str(id).is_ok()
                && agent.conversation_id.as_deref() == Some(id)
        })
        .collect();
    if matches.len() > 1 {
        return unavailable(RuntimeReason::AmbiguousTerminal);
    }
    if let Some((session, agent)) = matches.first() {
        if observations.incomplete {
            return unavailable(RuntimeReason::TerminalUnavailable);
        }
        let Some(instance_id) = agent.instance_id.as_ref().filter(|id| !id.is_empty()) else {
            return unavailable(RuntimeReason::TerminalUnavailable);
        };
        if session.is_empty() || agent.pane_id.as_str().is_empty() {
            return unavailable(RuntimeReason::TerminalUnavailable);
        }
        let status = match agent.status {
            AgentStatus::Starting => TerminalRuntimeStatus::Starting,
            AgentStatus::Working => TerminalRuntimeStatus::Working,
            AgentStatus::Idle => TerminalRuntimeStatus::Idle,
            AgentStatus::Blocked => TerminalRuntimeStatus::Blocked,
            AgentStatus::Completed => TerminalRuntimeStatus::Completed,
            AgentStatus::Unknown => TerminalRuntimeStatus::Unknown,
        };
        return SessionRuntime {
            source: RuntimeSource::Terminal,
            interaction: RuntimeInteraction::Terminal,
            status: Some(status),
            terminal: Some(RuntimeTerminal {
                session_id: session.clone(),
                pane_id: agent.pane_id.as_str().into(),
                instance_id: instance_id.clone(),
            }),
            reason: None,
        };
    }
    if native {
        SessionRuntime {
            source: RuntimeSource::Native,
            interaction: RuntimeInteraction::Native,
            status: None,
            terminal: None,
            reason: None,
        }
    } else if observations.incomplete {
        unavailable(RuntimeReason::TerminalUnavailable)
    } else {
        SessionRuntime {
            source: RuntimeSource::Unobserved,
            interaction: RuntimeInteraction::Native,
            status: None,
            terminal: None,
            reason: Some(RuntimeReason::NotLoaded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "00000000-0000-4000-8000-000000000001";
    fn agent(kind: &str, id: &str, status: AgentStatus) -> Agent {
        Agent {
            conversation_id: Some(id.into()),
            instance_id: Some("opaque-incarnation".into()),
            target: "fixture".into(),
            pane_id: crate::BackendPaneId::new("p1"),
            workspace_id: None,
            tab_id: None,
            kind: Some(kind.into()),
            display_agent: None,
            status,
            state_change_seq: None,
            shell_pid: None,
            pending_user_input: None,
        }
    }
    #[test]
    fn exact_provider_identity_carries_status_and_binding_not_approval() {
        let mut observations = Observations {
            agents: vec![("local".into(), agent("codex", ID, AgentStatus::Working))],
            incomplete: false,
        };
        let runtime = resolve(&observations, ID, false);
        assert_eq!(runtime.source, RuntimeSource::Terminal);
        assert_eq!(runtime.status, Some(TerminalRuntimeStatus::Working));
        assert_eq!(runtime.interaction, RuntimeInteraction::Terminal);
        assert_eq!(runtime.terminal.unwrap().instance_id, "opaque-incarnation");
        observations.agents[0].1.status = AgentStatus::Blocked;
        assert_eq!(
            resolve(&observations, ID, false).status,
            Some(TerminalRuntimeStatus::Blocked)
        );
        observations.agents[0].1.kind = Some("opencode".into());
        assert_eq!(
            resolve(&observations, ID, false).source,
            RuntimeSource::Unobserved
        );
        observations.agents[0].1.kind = Some("codex".into());
        observations.agents[0].1.conversation_id = Some(format!("{ID}-other"));
        assert_eq!(
            resolve(&observations, ID, false).source,
            RuntimeSource::Unobserved
        );
    }
    #[test]
    fn ambiguity_missing_incarnation_and_backend_failure_cannot_authorize_takeover() {
        let row = ("local".into(), agent("codex", ID, AgentStatus::Idle));
        let mut observations = Observations {
            agents: vec![row.clone(), row],
            incomplete: false,
        };
        assert_eq!(
            resolve(&observations, ID, false).reason,
            Some(RuntimeReason::AmbiguousTerminal)
        );
        observations.agents.pop();
        observations.agents[0].1.instance_id = None;
        assert_eq!(
            resolve(&observations, ID, false).interaction,
            RuntimeInteraction::Unavailable
        );
        observations.agents.clear();
        observations.incomplete = true;
        assert_eq!(
            resolve(&observations, ID, false).interaction,
            RuntimeInteraction::Unavailable
        );
        assert_eq!(
            resolve(&observations, ID, true).source,
            RuntimeSource::Native
        );
    }
    #[test]
    fn lost_terminal_observation_never_becomes_idle() {
        let observation = Observations::default();
        let result = resolve(&observation, ID, false);
        assert_eq!(result.source, RuntimeSource::Unobserved);
        assert_eq!(result.reason, Some(RuntimeReason::NotLoaded));
        assert_eq!(result.status, None);
    }
}
