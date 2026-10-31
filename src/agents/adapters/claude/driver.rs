//! Claude Code over its official ACP adapter (`claude-agent-acp`). The
//! session lifecycle and reducer are shared; the runner, environment and
//! muqun MCP entry are set by the runtime through [`AcpDriver::set_launch`]
//! and [`AcpDriver::set_mcp_servers`].
use serde_json::Value;

use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider};
use crate::agents::ports::agent::AgentFuture;

pub struct ClaudeProvider;
impl AcpProvider for ClaudeProvider {
    const KIND: &'static str = "claude";
    const LABEL: &'static str = "Claude Code";
    // Replaced by the runner's arguments (see `runtime::claude::launch`).
    const ARGS: &'static [&'static str] = &[];
    const REQUIRE_MESSAGE_ID: bool = false;
    const MODEL_SELECTION: bool = true;
    // Adapter notices (MCP start failures, hooks) stay out of the transcript.
    const SESSION_NOTICES: bool = true;
    const IDLE_RETIREMENT: bool = true;
    const PROVIDER_CONFIG: bool = false;
    const EFFORT_CONFIG: &'static str = "effort";
    // `npx` installs the adapter before it can answer; about a minute here.
    const INITIALIZE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
    fn recovery_snapshot<'a>(
        _binary: &'a str,
        directory: &'a str,
        id: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> AgentFuture<'a, crate::agents::adapters::acp::recovery::NativeSnapshot> {
        Box::pin(super::recovery::inspect(directory, id, stop))
    }
    fn offline_sessions<'a>(
        directory: Option<&'a str>,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> Option<AgentFuture<'a, Vec<Value>>> {
        Some(Box::pin(super::native_sessions::list(
            directory.map(str::to_owned),
            stop.clone(),
        )))
    }
    fn safe_error_message(error: &Value) -> Option<&'static str> {
        safe_error_message(error)
    }
}
pub type ClaudeDriver = AcpDriver<ClaudeProvider>;

/// Exact adapter messages only; arbitrary native diagnostics are never shown.
pub(crate) fn safe_error_message(error: &Value) -> Option<&'static str> {
    match (
        error.get("code").and_then(Value::as_i64),
        error.get("message").and_then(Value::as_str),
    ) {
        // claude-agent-acp 0.99.1 answers a prompt this way when Claude Code
        // has no login (`_auth/status_update` reports kind "none").
        (Some(-32000), Some("Authentication required")) => Some(
            "Claude Code is not signed in; run claude in a terminal on this computer and sign in",
        ),
        _ => None,
    }
}
