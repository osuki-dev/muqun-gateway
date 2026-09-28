//! Discovery and capability negotiation for Terminal and Harness planes.
//!
//! Provides structured runtime capability inspection across:
//! 1. The Terminal Plane: multiplexers, PTY backends, and terminal features.
//! 2. The Agent Harness Plane: multi-harness status, models, personas, and AI features.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{ordered_sessions, session_capabilities, session_metadata, AppState};

/// Status of a harness instance
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessStatus {
    Connected,
    Reachable,
    Offline,
    Disabled,
    NotInstalled,
    Unconfigured,
}

/// Metadata describing one discovered or configured AI harness
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessDiscoveryInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub status: HarnessStatus,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub models: Vec<HarnessModelInfo>,
    pub agents: Vec<HarnessAgentInfo>,
    pub features: HarnessFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessModelInfo {
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
pub struct HarnessAgentInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Convert domain ModelInfo into discovered HarnessModelInfo
pub fn model_info_to_harness_model(model: &crate::agents::domain::ModelInfo) -> HarnessModelInfo {
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

    HarnessModelInfo {
        id: model.id.clone(),
        name: model.name.clone(),
        provider_id: model.provider_id.clone(),
        supports_reasoning,
        reasoning_effort_tiers,
        extra: BTreeMap::new(),
    }
}

/// Convert domain AgentInfo into discovered HarnessAgentInfo
pub fn agent_info_to_harness_agent(agent: &crate::agents::domain::AgentInfo) -> HarnessAgentInfo {
    HarnessAgentInfo {
        id: agent.id.clone(),
        name: agent.name.clone(),
        description: agent.description.clone(),
        extra: BTreeMap::new(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessFeatures {
    pub streaming: bool,
    pub reasoning_effort: bool,
    pub model_selection: bool,
    pub tool_approvals: bool,
    pub worktrees: bool,
    pub revert: bool,
    pub inbox: bool,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// The Harness Plane discovery model
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessPlaneDiscovery {
    pub supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_harness: Option<String>,
    pub harnesses: Vec<HarnessDiscoveryInfo>,
    pub features: HarnessPlaneFeatures,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessPlaneFeatures {
    pub multi_harness: bool,
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
    pub agents: HarnessPlaneDiscovery,
    /// Backward-compatibility alias for `agents`
    pub agent: HarnessPlaneDiscovery,
    /// Backward-compatibility alias for `agents`
    pub harness: HarnessPlaneDiscovery,
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
    let harness = state.agent_runtime.discover_agents().await;
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
        agents: harness.clone(),
        agent: harness.clone(),
        harness,
        ssh: Some(ssh),
        extra: BTreeMap::new(),
    }
}

/// Build complete capability discovery response
pub async fn build_discovery(state: &AppState, _sealed: bool) -> Value {
    let planes = build_discovery_planes(state).await;
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
        "capabilities": legacy_capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_discovery_info_serializes_with_camel_case() {
        let info = HarnessDiscoveryInfo {
            id: "deepseek".to_string(),
            name: "DeepSeek Harness".to_string(),
            kind: "deepseek".to_string(),
            status: HarnessStatus::Connected,
            enabled: true,
            endpoint: Some("http://127.0.0.1:3080".to_string()),
            version: Some("0.1.0".to_string()),
            models: vec![
                HarnessModelInfo {
                    id: "deepseek-chat".to_string(),
                    name: "DeepSeek Chat (V3)".to_string(),
                    provider_id: "deepseek".to_string(),
                    supports_reasoning: false,
                    reasoning_effort_tiers: Vec::new(),
                    extra: BTreeMap::new(),
                },
                HarnessModelInfo {
                    id: "deepseek-reasoner".to_string(),
                    name: "DeepSeek Reasoner (R1)".to_string(),
                    provider_id: "deepseek".to_string(),
                    supports_reasoning: true,
                    reasoning_effort_tiers: vec!["low".to_string(), "high".to_string()],
                    extra: BTreeMap::new(),
                },
            ],
            agents: vec![HarnessAgentInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek Assistant".to_string(),
                description: None,
                extra: BTreeMap::new(),
            }],
            features: HarnessFeatures {
                streaming: true,
                reasoning_effort: true,
                model_selection: true,
                tool_approvals: true,
                worktrees: false,
                revert: false,
                inbox: false,
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
    fn multi_plane_discovery_serializes_terminal_harness_and_ssh() {
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
            agents: HarnessPlaneDiscovery {
                supported: true,
                active_harness: Some("deepseek".to_string()),
                harnesses: Vec::new(),
                features: HarnessPlaneFeatures {
                    multi_harness: true,
                    catalog_aggregation: true,
                    session_routing: true,
                },
            },
            agent: HarnessPlaneDiscovery {
                supported: true,
                active_harness: Some("deepseek".to_string()),
                harnesses: Vec::new(),
                features: HarnessPlaneFeatures {
                    multi_harness: true,
                    catalog_aggregation: true,
                    session_routing: true,
                },
            },
            harness: HarnessPlaneDiscovery {
                supported: true,
                active_harness: Some("deepseek".to_string()),
                harnesses: Vec::new(),
                features: HarnessPlaneFeatures {
                    multi_harness: true,
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
        assert_eq!(val["agents"]["activeHarness"], "deepseek");
        assert_eq!(val["agent"]["activeHarness"], "deepseek");
        assert_eq!(val["harness"]["activeHarness"], "deepseek");
        assert_eq!(val["agents"]["features"]["multiHarness"], true);
        assert_eq!(val["harness"]["features"]["sessionRouting"], true);
        assert_eq!(val["ssh"]["supported"], true);
        assert_eq!(val["ssh"]["tunnelSupported"], true);
        assert_eq!(val["ssh"]["pushTokenSupported"], true);
    }
}
