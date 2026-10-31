//! Google Antigravity's own ACP server, `agy acp`. The session lifecycle and
//! reducer are shared; this names its launch, login and native store.
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

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
    // A stored Google login answers in milliseconds; without one `agy`
    // opens a browser sign-in and waits for it.
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
    fn offline_sessions<'a>(
        directory: Option<&'a str>,
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
