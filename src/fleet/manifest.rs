//! The manifest model, the rules that derive `suitable_for`, and the service
//! that assembles it.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::peers::Peer;
use super::probes;
use crate::platform::discovery::{
    AgentAvailability, AgentDiscoveryInfo, TerminalBackendDiscoveryInfo,
};
use crate::tasks::domain::{now_ms, TaskState};
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

/// The most items of a list a peer's compact manifest keeps.
const PEER_LIST_CAP: usize = 64;
/// The longest string a peer's compact manifest keeps, in characters.
const PEER_TEXT_CAP: usize = 256;

/// A string (clipped to [`PEER_TEXT_CAP`]), number or bool; anything else is null.
fn scalar(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(text.chars().take(PEER_TEXT_CAP).collect()),
        Value::Number(_) | Value::Bool(_) => value.clone(),
        _ => Value::Null,
    }
}

/// [`Manifest::compact`] over another computer's `GET /api/fleet/self`
/// answer: the [`CompactManifest`] keys only, field by field (so a peer on
/// another gateway version still reads), each a [`scalar`], each list cut
/// at [`PEER_LIST_CAP`].
pub(crate) fn compact_json(full: Value) -> Value {
    let list = |value: &Value| -> Vec<Value> {
        let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
        items[..items.len().min(PEER_LIST_CAP)].to_vec()
    };
    let pick = |item: &Value, keys: &[&str]| -> Value {
        keys.iter()
            .map(|key| (key.to_string(), scalar(&item[*key])))
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let agents: Vec<Value> = list(&full["agents"])
        .iter()
        .map(|agent| {
            let mut compact = pick(agent, &["id", "kind", "status", "version"]);
            let modes = list(&agent["modes"])
                .iter()
                .map(|mode| scalar(&mode["id"]))
                .collect::<Vec<_>>();
            compact["modes"] = modes.into();
            let models = agent["models"].as_array().map_or(0, Vec::len);
            compact["modelCount"] = models.into();
            compact
        })
        .collect();
    let terminal: Vec<Value> = list(&full["terminal"])
        .iter()
        .map(|backend| {
            pick(
                backend,
                &["sessionId", "label", "kind", "connected", "version"],
            )
        })
        .collect();
    let toolchains: Vec<Value> = list(&full["toolchains"])
        .iter()
        .map(|toolchain| pick(toolchain, &["id", "version", "path"]))
        .collect();
    let mut compact = pick(
        &full,
        &[
            "machineId",
            "label",
            "hostname",
            "os",
            "arch",
            "gatewayVersion",
        ],
    );
    compact["agents"] = agents.into();
    compact["terminal"] = terminal.into();
    compact["toolchains"] = toolchains.into();
    compact["tasks"] = pick(&full["tasks"], &["active", "waiting"]);
    compact["suitableFor"] = list(&full["suitableFor"])
        .iter()
        .filter(|tag| tag.is_string())
        .map(scalar)
        .collect::<Vec<_>>()
        .into();
    compact
}

/// A linked computer, as `fleet.describe` lists it beside this one: what
/// this gateway last heard of it, never a call per field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerEntry {
    pub name: String,
    /// `null`: not checked in the last minute (a check is under way).
    pub reachable: Option<bool>,
    /// It no longer takes this computer's token: link it again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unauthorized: bool,
    /// When it last answered, ms since the epoch.
    pub last_seen: Option<u64>,
    pub capabilities: Vec<String>,
    /// From its cached manifest (compact); empty while none was fetched.
    pub agents: Vec<Value>,
    pub suitable_for: Vec<String>,
}

/// Why `fleet.describe` has nothing for a machine.
#[derive(Debug)]
pub(crate) enum DescribeError {
    /// Neither this computer nor a linked one; the names that are.
    Unknown(Vec<String>),
    /// A linked computer that does not answer now.
    Unreachable(String),
    /// A linked computer that answers but gave no manifest (none cached).
    NoManifest(String),
    /// A linked computer that no longer takes this computer's token.
    Unauthorized(String),
}

/// A linked computer as last heard, without asking it now.
struct Heard {
    peer: Peer,
    reachable: Option<bool>,
}

impl Heard {
    fn strings(&self, field: &str) -> Vec<String> {
        self.peer
            .manifest
            .as_ref()
            .map_or_else(Vec::new, |manifest| {
                manifest[field]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default()
            })
    }

    fn entry(&self) -> PeerEntry {
        PeerEntry {
            name: self.peer.name.clone(),
            reachable: self.reachable,
            unauthorized: self.peer.unauthorized,
            last_seen: self.peer.last_seen_ms,
            capabilities: self.peer.capabilities.clone(),
            agents: self
                .peer
                .manifest
                .as_ref()
                .and_then(|manifest| manifest["agents"].as_array().cloned())
                .unwrap_or_default(),
            suitable_for: self.strings("suitableFor"),
        }
    }

    fn summary(&self) -> MachineSummary {
        let manifest = self.peer.manifest.as_ref();
        MachineSummary {
            // What `fleet.describe` takes for it.
            machine_id: self.peer.server_id.clone(),
            label: self.peer.name.clone(),
            os: manifest
                .and_then(|manifest| manifest["os"].as_str())
                .unwrap_or_default()
                .to_owned(),
            online: self.reachable,
            suitable_for: self.strings("suitableFor"),
            tasks_active: manifest
                .and_then(|manifest| manifest["tasks"]["active"].as_u64())
                .unwrap_or(0) as usize,
        }
    }
}

/// Every linked computer as last heard, answered at once; those whose
/// reachability or manifest is stale are refreshed in the background (one
/// refresh per peer at a time), for the next call to see.
async fn heard(state: &AppState) -> Vec<Heard> {
    let peers = match state.peers.list().await {
        Ok(peers) => peers,
        Err(error) => {
            tracing::warn!("fleet: the peers could not be listed: {error:#}");
            return Vec::new();
        }
    };
    let now = now_ms();
    peers
        .into_iter()
        .map(|peer| {
            if peer.unauthorized {
                return Heard {
                    reachable: Some(false),
                    peer,
                };
            }
            state.peers.refresh_later(state, peer.clone());
            Heard {
                reachable: state.peers.reachability(&peer, now),
                peer,
            }
        })
        .collect()
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
    pub os: String,
    /// `null`: a linked computer not checked in the last minute.
    pub online: Option<bool>,
    pub suitable_for: Vec<String>,
    pub tasks_active: usize,
}

/// Whether `machine` names this computer: unnamed, `"self"` (any case), its
/// id, or its own label (any case; the agent knows it by the name the phone
/// shows) unless a linked computer of `peers` is called that: a peer's name
/// wins, and `self` stays the explicit way to say this one.
pub(crate) fn is_self(
    machine: Option<&str>,
    machine_id: &str,
    label: &str,
    peers: &[String],
) -> bool {
    machine.is_none_or(|machine| {
        super::peers::reserved_name(machine)
            || machine == machine_id
            || (super::peers::same_name(machine, label)
                && !peers
                    .iter()
                    .any(|peer| super::peers::same_name(peer, machine)))
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

    /// What `fleet.describe` answers for `machine`: this computer's compact
    /// manifest plus an entry per linked computer (`peers`), or a linked
    /// computer's own compact manifest (by name in any case, or either id):
    /// the one call that may wait on a peer, for one check and one fetch.
    pub(crate) async fn describe(
        &self,
        state: &AppState,
        machine: Option<&str>,
    ) -> Result<Value, DescribeError> {
        let (id, label) = (&state.config.server_id, &state.config.label);
        if is_self(machine, id, label, &state.peers.names()) {
            let (manifest, peers) = tokio::join!(self.manifest(state), heard(state));
            let mut answer = json!(manifest.compact());
            answer["peers"] = json!(peers.iter().map(Heard::entry).collect::<Vec<_>>());
            return Ok(answer);
        }
        let wanted = machine.unwrap_or_default();
        let unknown = || DescribeError::Unknown(state.peers.names());
        let peer = state.peers.resolve(wanted).await.ok_or_else(unknown)?;
        // Known to answer, with a manifest: that, at once; any refresh it
        // needs runs in the background, shared with every other caller.
        if state.peers.reachability(&peer, now_ms()) == Some(true) {
            if let Some(manifest) = peer.manifest.clone() {
                state.peers.refresh_later(state, peer);
                return Ok(manifest);
            }
        }
        let name = peer.name.clone();
        let Some(peer) = state.peers.checked(state, peer).await else {
            return Err(DescribeError::Unauthorized(name));
        };
        if !peer.reachable(now_ms()) {
            return Err(DescribeError::Unreachable(peer.name));
        }
        let manifest = state.peers.manifest_of(&peer).await;
        manifest.ok_or(DescribeError::NoManifest(peer.name))
    }

    /// What `fleet.list` answers: this computer, then every linked one.
    pub(crate) async fn list(&self, state: &AppState) -> Vec<MachineSummary> {
        let (manifest, peers) = tokio::join!(self.manifest(state), heard(state));
        let mut machines = vec![MachineSummary {
            machine_id: manifest.machine_id.clone(),
            label: manifest.label.clone(),
            os: manifest.os.to_owned(),
            online: Some(true),
            suitable_for: manifest.suitable_for.clone(),
            tasks_active: manifest.tasks.active,
        }];
        machines.extend(peers.iter().map(Heard::summary));
        machines
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
        for (machine, peers, expected) in [
            (None, &[][..], true),
            (Some("self"), &[], true),
            (Some("SELF"), &["self-ish"], true),
            (Some("srv-1"), &[], true),
            (Some("CODING"), &[], true),
            (Some("mini"), &["mini"], false),
            // A linked computer that shares this one's label wins it.
            (Some("Coding"), &["coding"], false),
            (Some("self"), &["coding"], true),
        ] {
            let peers: Vec<String> = peers.iter().map(|name| name.to_string()).collect();
            assert_eq!(
                is_self(machine, "srv-1", "coding", &peers),
                expected,
                "{machine:?}"
            );
        }
    }

    /// A peer's manifest keeps the compact keys only, model catalogs counted.
    #[test]
    fn a_peer_manifest_keeps_only_the_compact_keys() {
        let full = serde_json::json!({
            "machineId": "srv-mini", "os": "m".repeat(1000), "label": { "big": [1, 2] },
            "secret": "x", "suitableFor": ["web", 1],
            "agents": [{ "id": "opencode", "models": [{}, {}], "modes": [{ "id": "build", "name": "Build" }], "env": "x" }],
            "toolchains": (0..100).map(|i| serde_json::json!({ "id": format!("t{i}") })).collect::<Vec<_>>(),
        });
        let compact = compact_json(full);
        assert_eq!(compact.get("secret"), None);
        assert_eq!(compact["agents"][0]["modelCount"], 2);
        assert_eq!(compact["agents"][0]["modes"], serde_json::json!(["build"]));
        assert_eq!(compact["agents"][0].get("env"), None);
        assert_eq!(
            compact["toolchains"].as_array().unwrap().len(),
            PEER_LIST_CAP
        );
        assert_eq!(compact["suitableFor"], serde_json::json!(["web"]));
        assert_eq!(compact["os"].as_str().map(str::len), Some(PEER_TEXT_CAP));
        assert_eq!(compact["label"], Value::Null, "an object is no label");
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
