//! Discovery and capability negotiation for the Terminal and Agents planes.
//!
//! Provides structured runtime capability inspection across:
//! 1. The Terminal Plane: multiplexers, PTY backends, and terminal features.
//! 2. The Agents Plane: status of each agent, its models, modes, and features.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{ordered_sessions, session_capabilities, session_metadata, AppState};

/// Whether one agent can be used right now
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentAvailability {
    Connected,
    Reachable,
    Offline,
    Disabled,
    NotInstalled,
    Unconfigured,
}

/// Metadata describing one discovered or configured agent
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDiscoveryInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub status: AgentAvailability,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub models: Vec<AgentModelInfo>,
    pub modes: Vec<AgentModeInfo>,
    pub features: AgentFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelInfo {
    pub id: String,
    pub name: String,
    pub provider_id: String,
    pub supports_reasoning: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_effort_tiers: Vec<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModeInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Convert domain ModelInfo into discovered AgentModelInfo
pub fn model_info_to_agent_model(model: &crate::agents::domain::ModelInfo) -> AgentModelInfo {
    let mut reasoning_effort_tiers = Vec::new();
    let mut supports_reasoning = false;

    if let Some(ref variants) = model.variants {
        for v in variants {
            if let Some(ref effort) = v.reasoning_effort {
                supports_reasoning = true;
                if !reasoning_effort_tiers.contains(effort) {
                    reasoning_effort_tiers.push(effort.clone());
                }
            }
        }
    }

    AgentModelInfo {
        id: model.id.clone(),
        name: model.name.clone(),
        provider_id: model.provider_id.clone(),
        supports_reasoning,
        reasoning_effort_tiers,
        extra: BTreeMap::new(),
    }
}

/// Convert domain ModeInfo into discovered AgentModeInfo
pub fn mode_info_to_agent_mode(mode: &crate::agents::domain::ModeInfo) -> AgentModeInfo {
    AgentModeInfo {
        id: mode.id.clone(),
        name: mode.name.clone(),
        description: mode.description.clone(),
        extra: BTreeMap::new(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFeatures {
    pub streaming: bool,
    pub reasoning_effort: bool,
    pub model_selection: bool,
    pub tool_approvals: bool,
    pub worktrees: bool,
    pub revert: bool,
    /// Revert goes through `POST …/revert/stage` and commit; when false the
    /// agent only reverts in one step and staging answers `501`.
    pub staged_revert: bool,
    pub inbox: bool,
    /// The agent has modes (personas or presets) to pick from.
    pub modes: bool,
    pub skills: bool,
    pub slash_commands: bool,
    pub compaction: bool,
    pub background_shells: bool,
    pub attachments: bool,
    /// Attachments reach the agent as host paths listed in the prompt text,
    /// not as native file parts; the agent reads them with its own tools.
    pub attachments_by_path: bool,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// The Agents Plane discovery model
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPlaneDiscovery {
    pub supported: bool,
    pub agents: Vec<AgentDiscoveryInfo>,
    pub features: AgentPlaneFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPlaneFeatures {
    pub multi_agent: bool,
    pub catalog_aggregation: bool,
    pub session_routing: bool,
}

/// Terminal backend info in discovery
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalBackendDiscoveryInfo {
    pub session_id: String,
    pub label: String,
    pub kind: String,
    pub connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalFeatures {
    pub multi_window: bool,
    pub split_pane: bool,
    pub raw_pty: bool,
    pub pane_shortcuts: bool,
    pub pane_context: bool,
    pub git_diff: bool,
}

/// The Terminal Plane discovery model
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalPlaneDiscovery {
    pub supported: bool,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_backend: Option<String>,
    pub backends: Vec<TerminalBackendDiscoveryInfo>,
    pub features: TerminalFeatures,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded_reason: Option<String>,
}

/// The SSH Plane discovery model
///
/// Advertises the app-side transport, not a gateway subsystem: the phone can
/// open its own SSH connection to a host and tunnel the gateway's loopback
/// port. The gateway implements no SSH client or server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshPlaneDiscovery {
    pub supported: bool,
    pub tunnel_supported: bool,
    pub push_token_supported: bool,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Discovered multi-plane structure across Terminal, Agents, and SSH planes
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryPlanes {
    pub terminal: TerminalPlaneDiscovery,
    pub agents: AgentPlaneDiscovery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshPlaneDiscovery>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Build terminal plane discovery from current app state
pub async fn build_terminal_plane_discovery(state: &AppState) -> TerminalPlaneDiscovery {
    let ordered = ordered_sessions(state).await;
    let primary = ordered.first().copied();

    let mut backends = Vec::with_capacity(state.config.sessions.len());
    let mut any_connected = false;

    for session in &state.config.sessions {
        let (metadata, _) = session_metadata(session).await;
        let is_connected = metadata
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        any_connected |= is_connected;

        let capabilities = session_capabilities(
            session.backend,
            is_connected,
            metadata.get("version").and_then(Value::as_str),
        );

        backends.push(TerminalBackendDiscoveryInfo {
            session_id: session.id.clone(),
            label: session.label.clone(),
            kind: format!("{:?}", session.backend).to_lowercase(),
            connected: is_connected,
            version: metadata
                .get("version")
                .and_then(Value::as_str)
                .map(String::from),
            protocol: metadata
                .get("protocol")
                .and_then(Value::as_str)
                .map(String::from),
            capabilities: capabilities.into_iter().map(String::from).collect(),
        });
    }

    let supported = !state.config.sessions.is_empty();
    let degraded_reason = if state.config.sessions.is_empty() {
        Some("no_terminal_backend_configured".to_string())
    } else if !any_connected {
        Some("all_terminal_backends_disconnected".to_string())
    } else {
        None
    };

    let has_git_diff = backends
        .iter()
        .any(|b| b.capabilities.iter().any(|c| c == "git_diff"));
    let has_pane_context = backends
        .iter()
        .any(|b| b.capabilities.iter().any(|c| c == "pane_context"));
    let has_pty = backends
        .iter()
        .any(|b| b.kind == "tmux" || b.kind == "pty" || b.kind == "herdr");

    TerminalPlaneDiscovery {
        supported,
        mode: "auto".to_string(),
        active_backend: primary.map(|s| format!("{:?}", s.backend).to_lowercase()),
        backends,
        features: TerminalFeatures {
            multi_window: supported,
            split_pane: supported,
            raw_pty: has_pty,
            pane_shortcuts: supported,
            pane_context: has_pane_context,
            git_diff: has_git_diff,
        },
        degraded_reason,
    }
}

/// Build combined discovery planes
pub async fn build_discovery_planes(state: &AppState) -> DiscoveryPlanes {
    let terminal = build_terminal_plane_discovery(state).await;
    let agents = state.agent_runtime.discover_agents().await;
    // The SSH plane is the app's transport, not a gateway subsystem: the phone
    // opens its own SSH connection and tunnels this gateway's loopback port, so
    // the same HTTP API answers on the far side. These flags only tell the app
    // what that transport can carry; nothing here implements SSH.
    let ssh = SshPlaneDiscovery {
        supported: true,
        tunnel_supported: true,
        push_token_supported: true,
        extra: BTreeMap::new(),
    };
    DiscoveryPlanes {
        terminal,
        agents,
        ssh: Some(ssh),
        extra: BTreeMap::new(),
    }
}

impl AgentPlaneDiscovery {
    /// Drop what only a paired device should learn: where each agent
    /// listens and which version it runs.
    pub fn redact_endpoints(&mut self) {
        for agent in &mut self.agents {
            agent.endpoint = None;
            agent.version = None;
        }
    }
}

/// Build complete capability discovery response. An unauthenticated caller
/// gets each agent's id, kind, status and features, without endpoints or versions.
pub async fn build_discovery(state: &AppState, _sealed: bool, authenticated: bool) -> Value {
    let mut planes = build_discovery_planes(state).await;
    if !authenticated {
        planes.agents.redact_endpoints();
    }
    let collaboration_somewhere = planes.terminal.backends.iter().any(|b| {
        b.capabilities
            .iter()
            .any(|c| c == crate::AGENT_COLLABORATION_CAPABILITY)
    });
    let legacy_capabilities: Vec<String> = crate::gateway_capabilities(collaboration_somewhere)
        .into_iter()
        .map(String::from)
        .collect();

    json!({
        "ok": true,
        "gatewayVersion": env!("CARGO_PKG_VERSION"),
        "apiVersion": crate::GATEWAY_API_VERSION,
        "apiMajor": crate::GATEWAY_API_MAJOR,
        "platform": std::env::consts::OS,
        "serverId": state.config.server_id,
        "label": state.config.label,
        "planes": planes,
        // Not redacted for anyone: a path and a protocol number say nothing
        // an unauthenticated caller could not learn by trying the upgrade.
        "transports": transports_discovery(),
        "capabilities": legacy_capabilities,
    })
}

/// The App-facing transports beyond plain HTTP, keyed by name. An App that
/// does not know a key ignores it and keeps using HTTP and SSE.
pub fn transports_discovery() -> Value {
    json!({
        "websocket": {
            "path": crate::agents::ws_routes::WS_PATH,
            "protocol": crate::agents::ws_routes::WS_PROTOCOL,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_discovery_info_serializes_with_camel_case() {
        let info = AgentDiscoveryInfo {
            id: "deepseek".to_string(),
            name: "DeepSeek".to_string(),
            kind: "deepseek".to_string(),
            status: AgentAvailability::Connected,
            enabled: true,
            endpoint: Some("http://127.0.0.1:3080".to_string()),
            version: Some("0.1.0".to_string()),
            models: vec![
                AgentModelInfo {
                    id: "deepseek-chat".to_string(),
                    name: "DeepSeek Chat (V3)".to_string(),
                    provider_id: "deepseek".to_string(),
                    supports_reasoning: false,
                    reasoning_effort_tiers: Vec::new(),
                    extra: BTreeMap::new(),
                },
                AgentModelInfo {
                    id: "deepseek-reasoner".to_string(),
                    name: "DeepSeek Reasoner (R1)".to_string(),
                    provider_id: "deepseek".to_string(),
                    supports_reasoning: true,
                    reasoning_effort_tiers: vec!["low".to_string(), "high".to_string()],
                    extra: BTreeMap::new(),
                },
            ],
            modes: vec![AgentModeInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek Assistant".to_string(),
                description: None,
                extra: BTreeMap::new(),
            }],
            features: AgentFeatures {
                streaming: true,
                reasoning_effort: true,
                model_selection: true,
                tool_approvals: true,
                worktrees: false,
                revert: false,
                staged_revert: false,
                inbox: false,
                modes: true,
                skills: false,
                slash_commands: false,
                compaction: false,
                background_shells: false,
                attachments: true,
                attachments_by_path: true,
                extra: BTreeMap::new(),
            },
        };

        let val = serde_json::to_value(&info).expect("serializes");
        assert_eq!(val["id"], "deepseek");
        assert_eq!(val["status"], "connected");
        assert_eq!(val["models"][0]["supportsReasoning"], false);
        assert_eq!(val["models"][0]["providerId"], "deepseek");
        assert!(val["models"][0].get("reasoningEffortTiers").is_none());
        assert_eq!(val["models"][1]["supportsReasoning"], true);
        assert_eq!(
            val["models"][1]["reasoningEffortTiers"],
            json!(["low", "high"])
        );
        assert_eq!(val["features"]["reasoningEffort"], true);
        assert_eq!(val["features"]["attachments"], true);
        assert_eq!(val["features"]["attachmentsByPath"], true);
    }

    #[test]
    fn terminal_plane_discovery_serializes_with_camel_case() {
        let info = TerminalPlaneDiscovery {
            supported: true,
            mode: "auto".to_string(),
            active_backend: Some("tmux".to_string()),
            backends: vec![TerminalBackendDiscoveryInfo {
                session_id: "s1".to_string(),
                label: "Terminal 1".to_string(),
                kind: "tmux".to_string(),
                connected: true,
                version: Some("3.3a".to_string()),
                protocol: None,
                capabilities: vec!["agent_collaboration".to_string()],
            }],
            features: TerminalFeatures {
                multi_window: true,
                split_pane: true,
                raw_pty: true,
                pane_shortcuts: true,
                pane_context: true,
                git_diff: true,
            },
            degraded_reason: None,
        };

        let val = serde_json::to_value(&info).expect("serializes");
        assert_eq!(val["supported"], true);
        assert_eq!(val["activeBackend"], "tmux");
        assert_eq!(val["backends"][0]["sessionId"], "s1");
        assert_eq!(val["features"]["multiWindow"], true);
        assert_eq!(val["features"]["gitDiff"], true);
        assert!(val.get("degradedReason").is_none());
    }

    #[test]
    fn headless_terminal_plane_reports_degraded_reason() {
        let info = TerminalPlaneDiscovery {
            supported: false,
            mode: "none".to_string(),
            active_backend: None,
            backends: Vec::new(),
            features: TerminalFeatures {
                multi_window: false,
                split_pane: false,
                raw_pty: false,
                pane_shortcuts: false,
                pane_context: false,
                git_diff: false,
            },
            degraded_reason: Some("no_terminal_backend_configured".to_string()),
        };

        let val = serde_json::to_value(&info).expect("serializes");
        assert_eq!(val["supported"], false);
        assert_eq!(val["degradedReason"], "no_terminal_backend_configured");
    }

    #[test]
    fn multi_plane_discovery_serializes_terminal_agents_and_ssh() {
        let planes = DiscoveryPlanes {
            terminal: TerminalPlaneDiscovery {
                supported: true,
                mode: "auto".to_string(),
                active_backend: Some("tmux".to_string()),
                backends: Vec::new(),
                features: TerminalFeatures {
                    multi_window: true,
                    split_pane: true,
                    raw_pty: true,
                    pane_shortcuts: true,
                    pane_context: true,
                    git_diff: true,
                },
                degraded_reason: None,
            },
            agents: AgentPlaneDiscovery {
                supported: true,
                agents: Vec::new(),
                features: AgentPlaneFeatures {
                    multi_agent: true,
                    catalog_aggregation: true,
                    session_routing: true,
                },
            },
            ssh: Some(SshPlaneDiscovery {
                supported: true,
                tunnel_supported: true,
                push_token_supported: true,
                extra: BTreeMap::new(),
            }),
            extra: BTreeMap::new(),
        };

        let val = serde_json::to_value(&planes).expect("serializes");
        assert_eq!(val["terminal"]["supported"], true);
        // `agents` is the only agent plane key, and it names no active agent.
        let keys = |v: &Value| {
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&val), ["agents", "ssh", "terminal"]);
        assert_eq!(keys(&val["agents"]), ["agents", "features", "supported"]);
        assert_eq!(val["agents"]["agents"], json!([]));
        assert_eq!(val["agents"]["features"]["multiAgent"], true);
        assert_eq!(val["agents"]["features"]["sessionRouting"], true);
        assert_eq!(val["ssh"]["supported"], true);
        assert_eq!(val["ssh"]["tunnelSupported"], true);
        assert_eq!(val["ssh"]["pushTokenSupported"], true);
    }

    #[tokio::test]
    async fn discovery_announces_the_websocket_transport_to_everyone() {
        let state = crate::test_state("admin", Vec::new());
        for authenticated in [true, false] {
            let body = build_discovery(&state, false, authenticated).await;
            assert_eq!(
                body["transports"]["websocket"],
                json!({ "path": "/api/ws", "protocol": 1 })
            );
            assert!(body["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "ws_events"));
        }
    }

    #[tokio::test]
    async fn redaction_drops_endpoints_and_versions_but_keeps_status() {
        let mut plane = crate::agents::AgentRuntime::disabled()
            .discover_agents()
            .await;
        let id = plane.agents[0].id.clone();
        plane.agents[0].endpoint = Some("http://127.0.0.1:4096".into());
        plane.agents[0].version = Some("2.0.1".into());
        plane.redact_endpoints();
        let val = serde_json::to_value(&plane).unwrap();
        assert!(val["agents"][0].get("endpoint").is_none());
        assert!(val["agents"][0].get("version").is_none());
        assert_eq!(val["agents"][0]["status"], "disabled");
        assert_eq!(val["agents"][0]["id"], id.as_str());
    }
}
