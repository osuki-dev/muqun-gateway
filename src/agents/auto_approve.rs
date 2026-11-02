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
    /// Switchable on a live session through the agent's own protocol.
    Session,
    /// A process flag or environment of the agent: new processes only.
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
            scope: BypassScope::Launch,
        },
        "cursor" => NativeBypass {
            native: "--force",
            scope: BypassScope::Launch,
        },
        "fx" => NativeBypass {
            native: "full-access",
            scope: BypassScope::Launch,
        },
        "pi" => NativeBypass {
            native: "MUQUN_PI_APPROVALS=never",
            scope: BypassScope::Launch,
        },
        _ => return None,
    };
    Some(bypass)
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
        }),
        None => json!({ "scope": "none" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_names_its_own_control_or_none() {
        for (kind, native, scope) in [
            (
                "claude",
                Some("bypassPermissions"),
                Some(BypassScope::Session),
            ),
            ("qoder", Some("yolo"), Some(BypassScope::Session)),
            (
                "codex",
                Some("approvalPolicy=never"),
                Some(BypassScope::Session),
            ),
            (
                "opencode",
                Some("permission * allow"),
                Some(BypassScope::Session),
            ),
            ("t3", Some("full-access"), Some(BypassScope::Session)),
            (
                "antigravity",
                Some("--dangerously-skip-permissions"),
                Some(BypassScope::Launch),
            ),
            ("cursor", Some("--force"), Some(BypassScope::Launch)),
            ("fx", Some("full-access"), Some(BypassScope::Launch)),
            (
                "pi",
                Some("MUQUN_PI_APPROVALS=never"),
                Some(BypassScope::Launch),
            ),
            // Per-request approvals only: nothing to switch.
            ("deepseek", None, None),
            ("unknown", None, None),
        ] {
            let found = native_bypass(kind);
            assert_eq!(found.map(|b| b.native), native, "{kind}");
            assert_eq!(found.map(|b| b.scope), scope, "{kind}");
        }
    }

    #[test]
    fn the_feature_states_scope_and_native_name() {
        assert_eq!(feature("claude")["scope"], "session");
        assert_eq!(feature("claude")["native"], "bypassPermissions");
        assert_eq!(feature("antigravity")["scope"], "launch");
        assert_eq!(feature("deepseek"), json!({ "scope": "none" }));
    }
}
