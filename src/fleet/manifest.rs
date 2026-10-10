//! The manifest model, the rules that derive `suitable_for`, and the service
//! that assembles it.

use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;

use super::probes;
use crate::platform::discovery::{
    AgentAvailability, AgentDiscoveryInfo, TerminalBackendDiscoveryInfo,
};
use crate::tasks::domain::TaskState;
use crate::AppState;

const PROBE_TTL: Duration = Duration::from_secs(60);
/// The store caps a page at 1000, which bounds the counts too.
const COUNT_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
    pub machine_id: String,
    pub label: String,
    pub hostname: String,
    pub os: &'static str,
    pub arch: &'static str,
    pub gateway_version: &'static str,
    pub agents: Vec<AgentDiscoveryInfo>,
    pub terminal: Vec<TerminalBackendDiscoveryInfo>,
    pub toolchains: Vec<Toolchain>,
    pub tasks: TaskCounts,
    pub suitable_for: Vec<String>,
}

/// What `fleet.describe` answers: the manifest without what only an app
/// needs. An agent's model catalog alone runs to tens of kilobytes, so an
/// agent is its id, kind, status, version, mode ids and model count, and a
/// terminal backend leaves out its keyboard vocabulary and read features.
/// `GET /api/fleet/self` keeps the full [`Manifest`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactManifest {
    pub machine_id: String,
    pub label: String,
    pub hostname: String,
    pub os: &'static str,
    pub arch: &'static str,
    pub gateway_version: &'static str,
    pub agents: Vec<CompactAgent>,
    pub terminal: Vec<CompactTerminal>,
    pub toolchains: Vec<Toolchain>,
    pub tasks: TaskCounts,
    pub suitable_for: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactAgent {
    pub id: String,
    pub kind: String,
    pub status: AgentAvailability,
    pub version: Option<String>,
    pub modes: Vec<String>,
    pub model_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactTerminal {
    pub session_id: String,
    pub label: String,
    pub kind: String,
    pub connected: bool,
    pub version: Option<String>,
}

impl Manifest {
    pub(crate) fn compact(self) -> CompactManifest {
        CompactManifest {
            machine_id: self.machine_id,
            label: self.label,
            hostname: self.hostname,
            os: self.os,
            arch: self.arch,
            gateway_version: self.gateway_version,
            agents: self
                .agents
                .into_iter()
                .map(|agent| CompactAgent {
                    model_count: agent.models.len(),
                    modes: agent.modes.into_iter().map(|mode| mode.id).collect(),
                    id: agent.id,
                    kind: agent.kind,
                    status: agent.status,
                    version: agent.version,
                })
                .collect(),
            terminal: self
                .terminal
                .into_iter()
                .map(|backend| CompactTerminal {
                    session_id: backend.session_id,
                    label: backend.label,
                    kind: backend.kind,
                    connected: backend.connected,
                    version: backend.version,
                })
                .collect(),
            toolchains: self.toolchains,
            tasks: self.tasks,
            suitable_for: self.suitable_for,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Toolchain {
    pub id: String,
    pub version: Option<String>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCounts {
    pub active: usize,
    pub waiting: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MachineSummary {
    pub machine_id: String,
    pub label: String,
    pub os: &'static str,
    pub online: bool,
    pub suitable_for: Vec<String>,
    pub tasks_active: usize,
}

/// Whether `machine` names this computer: unnamed, `"self"`, its id, or its
/// own label in any case (the agent knows it by the name the phone shows).
pub(crate) fn is_self(machine: Option<&str>, machine_id: &str, label: &str) -> bool {
    machine.is_none_or(|machine| {
        machine == "self" || machine == machine_id || machine.eq_ignore_ascii_case(label)
    })
}

/// Simple, explainable rules over the stable layer.
pub(crate) fn suitable_for(
    os: &str,
    toolchains: &[Toolchain],
    agents_connected: bool,
    terminal_connected: bool,
) -> Vec<String> {
    let has = |id: &str| toolchains.iter().any(|t| t.id == id);
    let mut out = Vec::new();
    let mut add = |on: bool, tag: &str| {
        if on {
            out.push(tag.to_string());
        }
    };
    add(os == "macos" && has("xcode"), "ios-build");
    add(has("android-sdk") || has("adb"), "android-build");
    add(has("node") || has("bun"), "web");
    add(has("rust"), "rust");
    add(has("go"), "go");
    add(has("python3"), "python");
    add(has("docker"), "containers");
    add(agents_connected, "delegate");
    add(terminal_connected, "terminal");
    out
}

/// Holds the probe results, which are the only slow part of a manifest.
#[derive(Default)]
pub(crate) struct FleetService {
    probes: Mutex<Option<(Instant, Vec<Toolchain>)>>,
}

impl FleetService {
    async fn toolchains(&self) -> Vec<Toolchain> {
        let mut slot = self.probes.lock().await;
        if let Some((at, found)) = slot.as_ref() {
            if at.elapsed() < PROBE_TTL {
                return found.clone();
            }
        }
        let found = probes::run().await;
        *slot = Some((Instant::now(), found.clone()));
        found
    }

    async fn task_counts(state: &AppState) -> TaskCounts {
        let count = |states: Vec<TaskState>| async {
            match state.tasks.list_for(None, Some(states), COUNT_LIMIT).await {
                Ok(tasks) => tasks.len(),
                Err(error) => {
                    tracing::warn!(%error, "fleet task count failed");
                    0
                }
            }
        };
        TaskCounts {
            active: count(vec![
                TaskState::Dispatching,
                TaskState::Working,
                TaskState::IdleUnconfirmed,
            ])
            .await,
            waiting: count(vec![TaskState::Waiting]).await,
        }
    }

    pub(crate) async fn manifest(&self, state: &AppState) -> Manifest {
        let (planes, toolchains, hostname, tasks) = tokio::join!(
            crate::platform::discovery::build_discovery_planes(state),
            self.toolchains(),
            async {
                tokio::task::spawn_blocking(crate::hostname_label)
                    .await
                    .unwrap_or_default()
            },
            Self::task_counts(state),
        );
        let os = std::env::consts::OS;
        let agents_connected = planes
            .agents
            .agents
            .iter()
            .any(|a| a.status == AgentAvailability::Connected);
        let terminal_connected = planes.terminal.backends.iter().any(|b| b.connected);
        Manifest {
            machine_id: state.config.server_id.clone(),
            label: state.config.label.clone(),
            hostname,
            os,
            arch: std::env::consts::ARCH,
            gateway_version: env!("CARGO_PKG_VERSION"),
            suitable_for: suitable_for(os, &toolchains, agents_connected, terminal_connected),
            agents: planes.agents.agents,
            terminal: planes.terminal.backends,
            toolchains,
            tasks,
        }
    }

    /// The compact manifest of `machine`, if it names this computer.
    pub(crate) async fn describe(
        &self,
        state: &AppState,
        machine: Option<&str>,
    ) -> Option<CompactManifest> {
        if !is_self(machine, &state.config.server_id, &state.config.label) {
            return None;
        }
        Some(self.manifest(state).await.compact())
    }

    pub(crate) fn summary(m: &Manifest) -> MachineSummary {
        MachineSummary {
            machine_id: m.machine_id.clone(),
            label: m.label.clone(),
            os: m.os,
            online: true,
            suitable_for: m.suitable_for.clone(),
            tasks_active: m.tasks.active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc(ids: &[&str]) -> Vec<Toolchain> {
        ids.iter()
            .map(|id| Toolchain {
                id: id.to_string(),
                version: None,
                path: None,
            })
            .collect()
    }

    #[test]
    fn only_this_computer_is_self() {
        for (machine, expected) in [
            (None, true),
            (Some("self"), true),
            (Some("srv-1"), true),
            (Some("CODING"), true),
            (Some("mini"), false),
        ] {
            assert_eq!(is_self(machine, "srv-1", "coding"), expected, "{machine:?}");
        }
    }

    #[test]
    fn suitability_follows_the_toolchains() {
        let mac = suitable_for("macos", &tc(&["xcode", "node"]), true, false);
        assert_eq!(mac, ["ios-build", "web", "delegate"]);
        let linux = suitable_for("linux", &tc(&["xcode", "adb", "docker"]), false, true);
        assert_eq!(linux, ["android-build", "containers", "terminal"]);
        assert!(!linux.contains(&"ios-build".to_string()));
    }
}
