//! Each agent's own "auto-approve everything" control, by agent kind.
//!
//! The App's auto-approve switch is a request to put the agent itself into
//! its native bypass mode for one session -- never an App-side rule that
//! answers prompts on the agent's behalf. What that mode is, and whether it
//! can be switched on a live session, differs per agent; this table states
//! it, discovery advertises it (`features.autoApprove`), and the adapters
//! implement the session-scoped ones (`AgentPort::set_auto_approve`).
use serde_json::{json, Value};

/// Where the agent's bypass control lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassScope {
    /// Switchable on a live session: through the agent's own protocol, or
    /// (a launch flag) by relaunching that session's process with it.
    Session,
    /// A process flag or environment of the agent that no session switch
    /// reaches: new processes only.
    Launch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeBypass {
    /// The agent's own name for the control (a mode id, flag or setting).
    pub native: &'static str,
    pub scope: BypassScope,
}

/// The agent's native bypass control, or `None` for an agent that has
/// nothing beyond answering each request.
pub fn native_bypass(kind: &str) -> Option<NativeBypass> {
    let bypass = match kind {
        "claude" => NativeBypass {
            native: "bypassPermissions",
            scope: BypassScope::Session,
        },
        "qoder" => NativeBypass {
            native: "yolo",
            scope: BypassScope::Session,
        },
        "codex" => NativeBypass {
            native: "approvalPolicy=never",
            scope: BypassScope::Session,
        },
        "opencode" => NativeBypass {
            native: "permission * allow",
            scope: BypassScope::Session,
        },
        "t3" => NativeBypass {
            native: "full-access",
            scope: BypassScope::Session,
        },
        "antigravity" => NativeBypass {
            native: "--dangerously-skip-permissions",
            scope: BypassScope::Session,
        },
        "cursor" => NativeBypass {
            native: "--force",
            scope: BypassScope::Session,
        },
        "fx" => NativeBypass {
            native: "full-access",
            scope: BypassScope::Session,
        },
        "pi" => NativeBypass {
            native: "MUQUN_PI_APPROVALS=never",
            scope: BypassScope::Launch,
        },
        _ => return None,
    };
    Some(bypass)
}

/// The session modes that make an agent stop asking -- its bypass mode and
/// its "the agent decides" classifier mode -- by the ids its ACP
/// `modes.availableModes` uses. A device may select one of these (by the
/// mode route, on session creation, or through the auto-approve switch)
/// only where the owner allowed it (`allow_remote_auto_approve`).
pub fn elevating_modes(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &["bypassPermissions", "auto"],
        "qoder" => &["yolo", "bypassPermissions", "auto"],
        // Native Autopilot enables blanket permissions in addition to autonomy.
        "copilot" => &["https://agentclientprotocol.com/protocol/session-modes#autopilot"],
        _ => &[],
    }
}

pub fn is_elevating(kind: &str, mode: &str) -> bool {
    elevating_modes(kind).contains(&mode)
}

/// State each agent's native bypass in its discovery features.
pub fn annotate(agents: &mut [crate::discovery::AgentDiscoveryInfo]) {
    for agent in agents {
        agent
            .features
            .extra
            .insert("autoApprove".into(), feature(&agent.kind));
    }
}

/// What discovery states as `features.autoApprove`: the scope and the
/// native name, or `{"scope":"none"}` for an agent without one.
pub fn feature(kind: &str) -> Value {
    match native_bypass(kind) {
        Some(bypass) => json!({
            "scope": match bypass.scope {
                BypassScope::Session => "session",
                BypassScope::Launch => "launch",
            },
            "native": bypass.native,
            // Additive metadata: older clients still use scope/native.
            "applies": match kind {
                "codex" => "next_turn",
                "cursor" | "antigravity" | "fx" => "restart_when_idle",
                "pi" => "next_launch",
                _ => "immediate",
            },
            "modeIds": match kind {
                "claude" => &["bypassPermissions"][..],
                "qoder" => &["yolo", "bypassPermissions"][..],
                _ => &[][..],
            },
        }),
        None => json!({ "scope": "none" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevating_modes_are_the_bypass_and_classifier_modes_of_each_agent() {
        for (kind, mode, elevating) in [
            ("claude", "bypassPermissions", true),
            ("claude", "auto", true),
            ("claude", "acceptEdits", false),
            ("claude", "plan", false),
            ("qoder", "yolo", true),
            ("qoder", "bypassPermissions", true),
            ("qoder", "dontAsk", false),
            // OpenCode's modes are personas, never permission policies.
            ("opencode", "auto", false),
            ("codex", "bypassPermissions", false),
        ] {
            assert_eq!(is_elevating(kind, mode), elevating, "{kind} {mode}");
        }
    }
}
