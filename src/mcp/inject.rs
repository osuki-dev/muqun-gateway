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

/// The one sentence a terminal agent started for a task is given out of
/// band, since Claude Code and Codex do not act on a server's instructions:
/// Claude Code's appended system prompt, Codex's `developer_instructions`.
pub(crate) const TASK_SENTENCE: &str = "You were started for a Muqun task: when it is \
     finished call the muqun tool tasks.complete (outcome blocked if you need the user).";

/// The key of the per-session instruction a session agent's task session
/// carries ([`TASK_SENTENCE`]) while its task is open.
pub(crate) const SESSION_INSTRUCTION_KEY: &str = "muqun-task";

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

/// The launch arguments that give a `kind` agent started for a task
/// [`TASK_SENTENCE`] out of band: Claude Code reads it from `file` (else
/// from the argument) unless its configured launch arguments (`configured`)
/// already append a system prompt; Codex gets it after the user's own
/// `developer_instructions` (`own`, read from their config, never written)
/// for this run only. Nothing for any other agent.
pub(crate) fn task_args(
    kind: &str,
    configured: &[String],
    file: Option<&Path>,
    own: Option<&str>,
) -> Vec<String> {
    match kind {
        "claude" => {
            let appends = configured.iter().any(|arg| {
                let flag = arg.split('=').next().unwrap_or(arg);
                flag == "--append-system-prompt" || flag == "--append-system-prompt-file"
            });
            match (appends, file) {
                (true, _) => Vec::new(),
                (false, Some(file)) => vec![
                    "--append-system-prompt-file".to_owned(),
                    file.to_string_lossy().into_owned(),
                ],
                (false, None) => vec![
                    "--append-system-prompt".to_owned(),
                    TASK_SENTENCE.to_owned(),
                ],
            }
        }
        "codex" => {
            let text = match own.map(str::trim).filter(|own| !own.is_empty()) {
                // One line: the launch line is typed into a shell.
                Some(own) => format!("{own} {TASK_SENTENCE}"),
                None => TASK_SENTENCE.to_owned(),
            };
            vec![
                "-c".to_owned(),
                format!("developer_instructions={}", toml::Value::String(text)),
            ]
        }
        _ => Vec::new(),
    }
}

/// [`task_args`] for a task's agent about to start: Claude Code's file
/// written into the gateway's state directory, Codex's own
/// `developer_instructions` read (read-only) from `$CODEX_HOME/config.toml`.
pub(crate) async fn terminal_task_args(config: &crate::Config, kind: &str) -> Vec<String> {
    if !enabled(config) || !matches!(kind, "claude" | "codex") {
        return Vec::new();
    }
    let configured = crate::agents::tasks::agent_launch_args(kind, &[], &config.agent_launch_args);
    let file = if kind == "claude" {
        private_file("task-instruction.md", TASK_SENTENCE.as_bytes()).await
    } else {
        None
    };
    let own = if kind == "codex" {
        codex_developer_instructions().await
    } else {
        None
    };
    task_args(kind, &configured, file.as_deref(), own.as_deref())
}

/// The user's own Codex `developer_instructions`, if their config sets one.
async fn codex_developer_instructions() -> Option<String> {
    let home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))?;
    let text = tokio::fs::read_to_string(home.join("config.toml"))
        .await
        .ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    table
        .get("developer_instructions")
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
}

/// `<state dir>/<name>` holding `content`, written (0600) when it differs.
async fn private_file(name: &str, content: &[u8]) -> Option<std::path::PathBuf> {
    let path = crate::state_dir().ok()?.join(name);
    if tokio::fs::read(&path).await.ok().as_deref() != Some(content) {
        let (write, content) = (path.clone(), content.to_vec());
        let written =
            tokio::task::spawn_blocking(move || crate::write_secret_file(&write, &content)).await;
        if !matches!(written, Ok(Ok(()))) {
            tracing::debug!(
                name,
                "private launch file not written; its text goes inline"
            );
            return None;
        }
    }
    Some(path)
}

/// `<state dir>/mcp-claude.json`, written (0600) when its content differs.
async fn claude_file(url: &str, helper: &str) -> Option<std::path::PathBuf> {
    let text = serde_json::to_vec_pretty(&claude_config(url, helper)).ok()?;
    private_file("mcp-claude.json", &text).await
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
    // One registration at a time, so two delegations do not both mint.
    static REGISTERING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = REGISTERING.lock().await;
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
    // A token per directory: a registration elsewhere keeps its own.
    let label = runtime_label(directory);
    let previous: Vec<String> = match tokens.list().await {
        Ok(rows) => rows
            .into_iter()
            .filter(|row| row.binding == binding && row.label == label)
            .map(|row| row.id)
            .collect(),
        Err(_) => Vec::new(),
    };
    let issued = match tokens.issue(binding, label, NO_EXPIRY).await {
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
            // The service's answer may echo the request, token included.
            let error = redact_bearer(&error.to_string());
            tracing::warn!(error, "the agent did not take the muqun MCP server");
            let _ = tokens.revoke(&issued.session.token_id).await;
        }
    }
}

/// The label of the token registered for `directory`: [`RUNTIME_LABEL`]
/// and a short digest of the directory, so each directory's registration
/// replaces only its own token.
fn runtime_label(directory: Option<&str>) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(directory.unwrap_or("").as_bytes());
    let short: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{RUNTIME_LABEL} {short}")
}

/// `text` with any `Bearer <token>` cut to `Bearer …`.
fn redact_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("Bearer ") {
        out.push_str(&rest[..at + "Bearer ".len()]);
        out.push('…');
        rest = &rest[at + "Bearer ".len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || "-_.~+/=".contains(c)))
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// At startup with injection off: the tokens earlier registrations handed a
/// session agent's service are revoked, so a registration that outlived the
/// switch answers 401.
pub(crate) async fn revoke_when_off(config: &crate::Config, tokens: &dyn AssistantTokenStore) {
    if enabled(config) {
        return;
    }
    let Ok(rows) = tokens.list().await else {
        return;
    };
    for row in rows
        .into_iter()
        .filter(|row| row.label.starts_with(RUNTIME_LABEL))
    {
        let _ = tokens.revoke(&row.id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{test_state, FakeAgent, FakeMcp};

    #[tokio::test]
    async fn a_session_agent_is_given_the_server_once_per_directory() {
        let state = test_state("admin", Vec::new());
        let tokens = state.mcp_tokens.as_ref();
        let live = |label: String| async move {
            tokens
                .list()
                .await
                .unwrap()
                .into_iter()
                .filter(|row| row.label == label)
                .count()
        };
        let mut agent = FakeAgent::new("opencode");
        let mcp = std::sync::Arc::new(std::sync::Mutex::new(FakeMcp::default()));
        agent.mcp = Some(mcp.clone());
        // Not connected: one token for this directory, the server added.
        ensure_session_agent(&state.config, tokens, &agent, Some("/a")).await;
        assert_eq!(live(runtime_label(Some("/a"))).await, 1);
        assert_eq!(mcp.lock().unwrap().added, 1);
        // Connected: nothing minted, nothing added.
        ensure_session_agent(&state.config, tokens, &agent, Some("/a")).await;
        assert_eq!(mcp.lock().unwrap().added, 1);
        // Another directory gets its own token; the first one's stays.
        mcp.lock().unwrap().connected = false;
        ensure_session_agent(&state.config, tokens, &agent, Some("/b")).await;
        assert_eq!(live(runtime_label(Some("/a"))).await, 1);
        assert_eq!(live(runtime_label(Some("/b"))).await, 1);
        // Re-registering a directory replaces only its own token.
        mcp.lock().unwrap().connected = false;
        ensure_session_agent(&state.config, tokens, &agent, Some("/b")).await;
        assert_eq!(live(runtime_label(Some("/b"))).await, 1);
        // The service refusing: the token just minted is revoked.
        {
            let mut fake = mcp.lock().unwrap();
            fake.connected = false;
            fake.refuse = true;
        }
        ensure_session_agent(&state.config, tokens, &agent, Some("/c")).await;
        assert_eq!(live(runtime_label(Some("/c"))).await, 0);
        // Injection off: no call at all.
        let mut off = state.config.clone();
        off.mcp.inject_into_agents = false;
        mcp.lock().unwrap().refuse = false;
        ensure_session_agent(&off, tokens, &agent, Some("/d")).await;
        assert_eq!(live(runtime_label(Some("/d"))).await, 0);
        // A refusal that echoes the request does not log the token.
        assert_eq!(
            redact_bearer("bad config {\"Authorization\":\"Bearer abc.DEF-123\"}"),
            "bad config {\"Authorization\":\"Bearer …\"}"
        );
    }

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

        // A task's agent gets the one sentence out of band; the user's own
        // system prompt and developer instructions win or stay.
        let sentence = TASK_SENTENCE;
        let own_prompt = ["--append-system-prompt-file".to_owned(), "/mine".to_owned()];
        let file = Path::new("/state/task-instruction.md");
        for (kind, configured, own, args) in [
            (
                "claude",
                &[][..],
                None,
                vec![
                    "--append-system-prompt-file".to_owned(),
                    "/state/task-instruction.md".to_owned(),
                ],
            ),
            ("claude", &own_prompt[..], None, vec![]),
            (
                "codex",
                &[][..],
                None,
                vec![
                    "-c".to_owned(),
                    format!("developer_instructions=\"{sentence}\""),
                ],
            ),
            (
                "codex",
                &[][..],
                Some("Answer in French."),
                vec![
                    "-c".to_owned(),
                    format!("developer_instructions=\"Answer in French. {sentence}\""),
                ],
            ),
            ("gemini", &[][..], None, vec![]),
        ] {
            assert_eq!(
                task_args(kind, configured, Some(file), own),
                args,
                "{kind} {own:?}"
            );
        }
    }
}
