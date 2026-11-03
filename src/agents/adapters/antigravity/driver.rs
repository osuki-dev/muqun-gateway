//! Google Antigravity's own ACP server, `agy acp`. The session lifecycle and
//! reducer are shared; this names its launch, login and native store.
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::mapper::PermissionFields;
use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider, Authenticate};
use crate::agents::ports::agent::AgentFuture;

pub(crate) const NOT_SIGNED_IN: &str =
    "Antigravity is not signed in on this computer; run `agy` in a terminal and sign in";

pub struct AntigravityProvider;
impl AcpProvider for AntigravityProvider {
    const KIND: &'static str = "antigravity";
    const LABEL: &'static str = "Antigravity";
    const ARGS: &'static [&'static str] = &["acp"];
    const REQUIRE_MESSAGE_ID: bool = false;
    const MODEL_SELECTION: bool = true;
    const IDLE_RETIREMENT: bool = true;
    const PROVIDER_CONFIG: bool = false;
    const EFFORT_CONFIG: &'static str = "thought_level";
    // agy's `run_command` asks with
    // `rawInput: {CommandLine, WaitMsBeforeAsync, toolSummary}` (agy 1.4);
    // the ACP `command` stays accepted. agy names the tool in the request's
    // `title`, not `name` (`run_command` recorded live); the other tools are
    // the ones its binary's tool schemas name (agy 1.4, read from the
    // binary, not verified live) -- where agy states a `kind` of its own,
    // that kind wins.
    const PERMISSION_FIELDS: PermissionFields = PermissionFields {
        shell_command: &["CommandLine", "command"],
        tool_actions: &[
            ("view_file", "read"),
            ("list_dir", "list"),
            ("find_by_name", "glob"),
            ("grep_search", "grep"),
            ("read_url_content", "webfetch"),
            ("write_to_file", "write"),
            ("replace_file_content", "edit"),
            ("multi_replace_file_content", "edit"),
        ],
    };
    // agy 1.4.12 advertises `_meta.antigravity.systemPrompt.append`, but its
    // wire shape is undocumented and unverified; until it is, the task
    // sentence goes in the prompt.
    const TASK_SENTENCE_IN_PROMPT: bool = true;
    // Every child pays `authenticate` before `session/load`, which agy
    // answers only after two Google API calls (2.0-2.7 s measured): a cold
    // open is ~2.5-3 s, the load itself 0.1-0.4 s. So a released child is
    // kept an hour, not five minutes, at ~350 MB resident each
    // (docs/antigravity-protocol.md, "Idle retirement").
    const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
    // Without a stored login `agy` opens a browser sign-in and waits for it.
    const AUTHENTICATE: Option<Authenticate> = Some(Authenticate {
        method: "oauth-personal",
        timeout: Duration::from_secs(20),
        refusal: NOT_SIGNED_IN,
    });
    fn recovery_snapshot<'a>(
        _binary: &'a str,
        _directory: &'a str,
        id: &'a str,
        stop: &'a CancellationToken,
    ) -> AgentFuture<'a, NativeSnapshot> {
        Box::pin(super::store::recovery_snapshot(id, stop))
    }
    const STORE_ACTIVITY: bool = true;
    fn transcript_modified(_directory: &str, id: &str) -> Option<std::time::SystemTime> {
        super::store::store_modified(id)
    }
    fn offline_sessions<'a>(
        directory: Option<&'a str>,
        _owned: std::collections::HashSet<String>,
        stop: &'a CancellationToken,
    ) -> Option<AgentFuture<'a, Vec<Value>>> {
        Some(Box::pin(super::store::list(directory, stop)))
    }
    fn safe_error_message(error: &Value) -> Option<&'static str> {
        safe_error_message(error)
    }
}
pub type AntigravityDriver = AcpDriver<AntigravityProvider>;

/// Exact `agy` 1.4 refusals only; other native text never reaches the App.
pub(crate) fn safe_error_message(error: &Value) -> Option<&'static str> {
    match (
        error.get("code").and_then(Value::as_i64),
        error.get("message").and_then(Value::as_str),
    ) {
        (Some(-32000), Some("Authentication required"))
        | (Some(-32800), Some("Sign-in was cancelled.")) => Some(NOT_SIGNED_IN),
        _ => None,
    }
}
