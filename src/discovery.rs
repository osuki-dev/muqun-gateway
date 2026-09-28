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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessAgentInfo {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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

/// Discovered dual-plane structure
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryPlanes {
    pub terminal: TerminalPlaneDiscovery,
    pub harness: HarnessPlaneDiscovery,
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

    TerminalPlaneDiscovery {
        supported,
        mode: "auto".to_string(),
        active_backend: primary.map(|s| format!("{:?}", s.backend).to_lowercase()),
        backends,
        features: TerminalFeatures {
            multi_window: true,
            split_pane: true,
            raw_pty: true,
            pane_shortcuts: true,
            pane_context: true,
            git_diff: true,
        },
        degraded_reason,
    }
}

/// Build combined discovery planes
pub async fn build_discovery_planes(state: &AppState) -> DiscoveryPlanes {
    let terminal = build_terminal_plane_discovery(state).await;
    let harness = state.agent_runtime.discover_harnesses().await;
    DiscoveryPlanes {
        terminal,
        harness,
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
            models: vec![HarnessModelInfo {
                id: "deepseek-chat".to_string(),
                name: "DeepSeek Chat (V3)".to_string(),
                provider_id: "deepseek".to_string(),
                supports_reasoning: false,
            }],
            agents: vec![HarnessAgentInfo {
                id: "deepseek".to_string(),
                name: "DeepSeek Assistant".to_string(),
                description: None,
            }],
            features: HarnessFeatures {
                streaming: true,
                reasoning_effort: true,
                model_selection: true,
                tool_approvals: true,
                worktrees: false,
                revert: false,
                inbox: false,
            },
        };

        let val = serde_json::to_value(&info).expect("serializes");
        assert_eq!(val["id"], "deepseek");
        assert_eq!(val["status"], "connected");
        assert_eq!(val["models"][0]["supportsReasoning"], false);
        assert_eq!(val["models"][0]["providerId"], "deepseek");
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
}
