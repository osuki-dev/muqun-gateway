//! The muqun server handed to the agents the gateway starts, at launch:
//! the same entry `muqun-gateway mcp enable <host>` would write, given to
//! the process (or, for OpenCode, to the running service) instead of to
//! the user's configuration. Nothing here writes a user file.
//!
//! - Claude Code: `--mcp-config <state dir>/mcp-claude.json` (it adds to the
//!   user's servers; `--strict-mcp-config` is never passed). The file holds
//!   the server's URL and the token helper command, no token, and is the
//!   same for every launch.
//! - Codex: `-c mcp_servers.muqun.url=…` and
//!   `-c mcp_servers.muqun.http_headers_helper=…`, which override those two
//!   keys for this run and leave the user's other servers as they are.
//! - OpenCode: the service's runtime API (`PUT /api/experimental/mcp/muqun`
//!   for the task's directory) with an agent-bound token, until the service
//!   restarts; the gateway registers it again before a delegation when it
//!   is not connected. Code Mode is off for it, so `tasks.complete` is a
//!   tool the model sees, not a catalog entry.
//!
//! Terminal agents authenticate through the helper's pane-bound token, as
//! with `mcp enable`. Off with `mcp.inject_into_agents: false`, or with
//! `mcp.enabled: false`.

use std::path::Path;

use serde_json::{json, Value};

use super::hosts::{self, Host, SERVER_NAME};
use crate::agents::ports::agent::AgentPort;
use crate::mcp::tokens::{AssistantTokenStore, TokenBinding, NO_EXPIRY};

/// The label of the tokens minted for OpenCode's runtime registration; an
/// earlier one is revoked when a new one is in use.
pub(crate) const RUNTIME_LABEL: &str = "opencode (started tasks)";

/// Whether agents the gateway starts get the muqun server: MCP on, the
/// switch on, and `/mcp` reachable on this computer's loopback.
pub(crate) fn enabled(config: &crate::Config) -> bool {
    config.mcp.enabled
        && config.mcp.inject_into_agents
        && super::loopback_warning(&config.listen).is_none()
}

/// The launch arguments that give the `kind` agent the server at `url`,
/// authenticating with `helper`; Claude Code reads its entry from
/// `claude_file` when there is one, else from the argument itself.
pub(crate) fn launch_args(
    kind: &str,
    url: &str,
    helper: &str,
    claude_file: Option<&Path>,
) -> Vec<String> {
    match kind {
        "claude" => vec![
            "--mcp-config".to_owned(),
            match claude_file {
                Some(file) => file.to_string_lossy().into_owned(),
                None => claude_config(url, helper).to_string(),
            },
        ],
        "codex" => vec![
            "-c".to_owned(),
            format!("mcp_servers.{SERVER_NAME}.url={}", Value::from(url)),
            "-c".to_owned(),
            format!(
                "mcp_servers.{SERVER_NAME}.http_headers_helper={}",
                Value::from(helper)
            ),
        ],
        _ => Vec::new(),
    }
}

fn claude_config(url: &str, helper: &str) -> Value {
    json!({ "mcpServers": { SERVER_NAME: Host::Claude.entry(url, helper) } })
}

/// [`launch_args`] for an agent the gateway is about to start in a pane,
/// writing Claude Code's file into the gateway's state directory first.
pub(crate) async fn terminal_args(config: &crate::Config, kind: &str) -> Vec<String> {
    if !enabled(config) || !matches!(kind, "claude" | "codex") {
        return Vec::new();
    }
    let url = super::local_url(&config.listen);
    let helper = hosts::helper_for_this_gateway();
    let file = if kind == "claude" {
        claude_file(&url, &helper).await
    } else {
        None
    };
    launch_args(kind, &url, &helper, file.as_deref())
}

/// `<state dir>/mcp-claude.json`, written (0600) when its content differs.
async fn claude_file(url: &str, helper: &str) -> Option<std::path::PathBuf> {
    let path = crate::state_dir().ok()?.join("mcp-claude.json");
    let text = serde_json::to_vec_pretty(&claude_config(url, helper)).ok()?;
    if tokio::fs::read(&path).await.ok().as_deref() != Some(text.as_slice()) {
        let write = path.clone();
        let written =
            tokio::task::spawn_blocking(move || crate::write_secret_file(&write, &text)).await;
        if !matches!(written, Ok(Ok(()))) {
            tracing::debug!("Claude Code's MCP file not written; its entry goes inline");
            return None;
        }
    }
    Some(path)
}

/// Before a delegation to `agent`'s session in `directory`: register the
/// muqun server with the service when it takes one at runtime and does not
/// have it connected. A failure is logged; the task goes on (the marker
/// still ends it).
pub(crate) async fn ensure_session_agent(
    config: &crate::Config,
    tokens: &dyn AssistantTokenStore,
    agent: &dyn AgentPort,
    directory: Option<&str>,
) {
    if !enabled(config) || !agent.adds_mcp_servers() {
        return;
    }
    match agent.mcp_server_connected(SERVER_NAME, directory).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            tracing::debug!(%error, "could not read the agent's MCP servers");
            return;
        }
    }
    let binding = TokenBinding::Agent {
        agent_id: agent.kind().to_owned(),
    };
    let previous: Vec<String> = match tokens.list().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| row.binding == binding && row.label == RUNTIME_LABEL)
            .map(|row| row.id)
            .collect(),
        Err(_) => Vec::new(),
    };
    let issued = match tokens.issue(binding, RUNTIME_LABEL.into(), NO_EXPIRY).await {
        Ok(issued) => issued,
        Err(error) => {
            tracing::warn!(%error, "could not mint the agent's MCP token");
            return;
        }
    };
    let server = json!({
        "type": "remote",
        "url": super::local_url(&config.listen),
        "headers": { "Authorization": format!("Bearer {}", issued.token) },
        "oauth": false,
        "codemode": false,
    });
    match agent.add_mcp_server(SERVER_NAME, &server, directory).await {
        Ok(()) => {
            for id in previous {
                let _ = tokens.revoke(&id).await;
            }
        }
        Err(error) => {
            tracing::warn!(%error, "the agent did not take the muqun MCP server");
            let _ = tokens.revoke(&issued.session.token_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_agents_get_the_server_on_their_launch_line_only() {
        let url = "http://127.0.0.1:23847/mcp";
        let helper = "muqun-gateway mcp token-helper";
        let file = Path::new("/state/mcp-claude.json");
        for (kind, file, args) in [
            (
                "claude",
                Some(file),
                vec!["--mcp-config", "/state/mcp-claude.json"],
            ),
            (
                "codex",
                None,
                vec![
                    "-c",
                    "mcp_servers.muqun.url=\"http://127.0.0.1:23847/mcp\"",
                    "-c",
                    "mcp_servers.muqun.http_headers_helper=\"muqun-gateway mcp token-helper\"",
                ],
            ),
            ("gemini", None, vec![]),
        ] {
            assert_eq!(launch_args(kind, url, helper, file), args, "{kind}");
        }
        // Inline, Claude Code's entry is what `mcp enable claude` writes.
        let inline = launch_args("claude", url, helper, None);
        let entry: Value = serde_json::from_str(&inline[1]).unwrap();
        assert_eq!(entry["mcpServers"]["muqun"]["headersHelper"], helper);
        assert!(!inline.iter().any(|arg| arg.contains("strict")));
    }
}
