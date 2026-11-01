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
    // Permission modes are the session's own (`session/set_mode`; a load
    // continues in the transcript's last mode).
    const SESSION_MODES: bool = true;
    const STEERING: bool = true;
    // AskUserQuestion arrives as a form elicitation.
    const FORM_ELICITATION: bool = true;
    const SYSTEM_PROMPT: bool = true;
    // `_session/rename` is offered to JetBrains AIR clients only.
    const RENAME_COMMAND: Option<&'static str> = Some("/rename");
    // `session/delete` is the SDK's transcript delete, which finds a saved
    // session without loading it (adapter 0.99.1).
    const DELETE_LISTED: bool = true;
    fn recovery_snapshot<'a>(
        _binary: &'a str,
        directory: &'a str,
        id: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> AgentFuture<'a, crate::agents::adapters::acp::recovery::NativeSnapshot> {
        Box::pin(super::recovery::inspect(directory, id, stop))
    }
    fn live_timeline<'a>(
        directory: &'a str,
        id: &'a str,
        stop: &'a tokio_util::sync::CancellationToken,
    ) -> AgentFuture<'a, Vec<crate::agents::domain::TimelineItem>> {
        Box::pin(super::recovery::live_timeline(directory, id, stop))
    }
    fn transcript_model<'a>(
        directory: &'a str,
        id: &'a str,
    ) -> AgentFuture<'a, Option<(String, Option<String>)>> {
        Box::pin(super::recovery::transcript_model(directory, id))
    }
    fn transcript_usage<'a>(
        directory: &'a str,
        id: &'a str,
    ) -> AgentFuture<'a, Option<crate::agents::domain::TokensUsage>> {
        Box::pin(super::recovery::transcript_usage(directory, id))
    }
    fn static_modes() -> Vec<crate::agents::domain::ModeInfo> {
        permission_modes()
    }
    fn work_settled(_directory: &str, id: &str, process: u32) -> bool {
        super::native_sessions::idle_under(id, process)
    }
    fn starts_background_work(update: &Value) -> bool {
        matches!(
            update["sessionUpdate"].as_str(),
            Some("tool_call" | "tool_call_update")
        ) && update["rawInput"]["run_in_background"].as_bool() == Some(true)
    }
    fn transcript_modified(directory: &str, id: &str) -> Option<std::time::SystemTime> {
        super::recovery::transcript_modified(directory, id)
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

/// The permission modes claude-agent-acp 0.99.1 offers every session
/// (`modes.availableModes`, ids and names as it reports them). It offers
/// `bypassPermissions` only where Claude Code accepts it ([`bypass_allowed`]);
/// a live session's own list replaces this one.
pub(crate) fn permission_modes() -> Vec<crate::agents::domain::ModeInfo> {
    let root = unsafe { libc::geteuid() } == 0;
    let settings: Vec<Value> = settings_files()
        .iter()
        .filter_map(|path| std::fs::read(path).ok())
        .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
        .collect();
    let bypass = bypass_allowed(root, std::env::var("IS_SANDBOX").ok().as_deref(), &settings);
    [
        ("default", "Manual", "Always ask before making changes"),
        (
            "acceptEdits",
            "Accept edits",
            "Automatically accept all file edits",
        ),
        ("plan", "Plan", "Create a plan before making changes"),
        ("auto", "Auto", "Claude handles permission decisions"),
        (
            "bypassPermissions",
            "Bypass permissions",
            "Accepts all permissions",
        ),
    ]
    .into_iter()
    .filter(|(id, _, _)| bypass || *id != "bypassPermissions")
    .map(|(id, name, description)| crate::agents::domain::ModeInfo {
        id: id.into(),
        name: name.into(),
        model: None,
        description: Some(description.into()),
        mode: None,
        color: None,
        hidden: false,
    })
    .collect()
}

/// Claude Code's user and managed settings, the tiers that apply to every
/// session directory. A project's own settings can only disable bypass
/// further; its live mode list says so.
fn settings_files() -> Vec<std::path::PathBuf> {
    let managed = if cfg!(target_os = "macos") {
        "/Library/Application Support/ClaudeCode/managed-settings.json"
    } else {
        "/etc/claude-code/managed-settings.json"
    };
    super::native_sessions::config_dir()
        .map(|dir| dir.join("settings.json"))
        .into_iter()
        .chain([managed.into()])
        .collect()
}

/// Whether Claude Code accepts `bypassPermissions`: never when any settings
/// tier sets `permissions.disableBypassPermissionsMode` to `"disable"`, and
/// as root only inside a sandbox it is told of with `IS_SANDBOX=1`.
fn bypass_allowed(root: bool, is_sandbox: Option<&str>, settings: &[Value]) -> bool {
    let disabled = settings.iter().any(|settings| {
        settings["permissions"]["disableBypassPermissionsMode"].as_str() == Some("disable")
    });
    !disabled && (!root || is_sandbox == Some("1"))
}

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

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn bypass_is_offered_only_where_claude_code_accepts_it() {
        let disabled = json!({"permissions": {"disableBypassPermissionsMode": "disable"}});
        let other = json!({"permissions": {"defaultMode": "plan"}});
        for (root, sandbox, settings, want) in [
            (false, None, vec![], true),
            (false, None, vec![other.clone()], true),
            // Any tier that disables it wins (user or managed).
            (false, None, vec![other.clone(), disabled.clone()], false),
            (true, None, vec![], false),
            (true, Some("1"), vec![], true),
            // Claude Code refuses root bypass for any other value.
            (true, Some("true"), vec![], false),
            (true, Some(""), vec![], false),
            (true, Some("1"), vec![disabled.clone()], false),
        ] {
            assert_eq!(
                super::bypass_allowed(root, sandbox, &settings),
                want,
                "{root} {sandbox:?} {settings:?}"
            );
        }
    }
}
