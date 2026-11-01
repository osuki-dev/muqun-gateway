//! Cursor's own ACP server, `agent acp` (Cursor CLI). The session lifecycle
//! and reducer are shared; this names its launch, login check and native store.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agents::adapters::acp::recovery::NativeSnapshot;
use crate::agents::adapters::acp::session::{AcpDriver, AcpProvider, Authenticate};
use crate::agents::ports::agent::{AgentError, AgentFuture};

pub(crate) const NOT_SIGNED_IN: &str =
    "Cursor is not signed in on this computer; run `agent login` in a terminal";
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);
const STATUS_OUTPUT_LIMIT: u64 = 64 * 1024;
/// A confirmed login is trusted this long before `agent status` runs again.
const SIGNED_IN_TTL: Duration = Duration::from_secs(600);

pub struct CursorProvider;
impl AcpProvider for CursorProvider {
    const KIND: &'static str = "cursor";
    const LABEL: &'static str = "Cursor";
    const ARGS: &'static [&'static str] = &["acp"];
    const REQUIRE_MESSAGE_ID: bool = false;
    // Effort is part of each Cursor model value (`name[reasoning=…]`); there
    // is no separate effort option.
    const MODEL_SELECTION: bool = true;
    const IDLE_RETIREMENT: bool = true;
    const PROVIDER_CONFIG: bool = false;
    // No system prompt in its ACP server, and MCP server instructions do not
    // reach the model.
    const TASK_SENTENCE_IN_PROMPT: bool = true;
    // Sessions are refused until `authenticate`. With a stored login it
    // answers in seconds; without one it opens a browser, so `signed_in`
    // runs first and this timeout only bounds a slow backend.
    const AUTHENTICATE: Option<Authenticate> = Some(Authenticate {
        method: "cursor_login",
        timeout: Duration::from_secs(90),
        refusal: NOT_SIGNED_IN,
    });
    fn signed_in<'a>(
        binary: &'a str,
        _stop: &'a CancellationToken,
    ) -> Option<AgentFuture<'a, bool>> {
        Some(Box::pin(signed_in(binary)))
    }
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
pub type CursorDriver = AcpDriver<CursorProvider>;

/// Exact Cursor CLI refusals only; other native text never reaches the App.
pub(crate) fn safe_error_message(error: &Value) -> Option<&'static str> {
    match (
        error.get("code").and_then(Value::as_i64),
        error.get("message").and_then(Value::as_str),
    ) {
        (Some(-32000), Some("Authentication required")) => Some(NOT_SIGNED_IN),
        _ => None,
    }
}

fn confirmed() -> &'static Mutex<HashMap<String, tokio::time::Instant>> {
    static CONFIRMED: std::sync::OnceLock<Mutex<HashMap<String, tokio::time::Instant>>> =
        std::sync::OnceLock::new();
    CONFIRMED.get_or_init(Default::default)
}

/// `agent status --format json` reads the stored login and never starts a
/// sign-in. Only a positive answer is remembered.
async fn signed_in(binary: &str) -> Result<bool, AgentError> {
    if confirmed()
        .lock()
        .unwrap()
        .get(binary)
        .is_some_and(|at| at.elapsed() < SIGNED_IN_TTL)
    {
        return Ok(true);
    }
    let status = login_status(binary).await.ok_or_else(|| {
        AgentError::NotAvailable(
            "Could not check the Cursor login; check that `agent status` runs on this computer"
                .into(),
        )
    })?;
    if status {
        confirmed()
            .lock()
            .unwrap()
            .insert(binary.to_owned(), tokio::time::Instant::now());
    }
    Ok(status)
}

async fn login_status(binary: &str) -> Option<bool> {
    use tokio::io::AsyncReadExt as _;
    let mut child = tokio::process::Command::new(binary)
        .args(["status", "--format", "json"])
        .env("NO_OPEN_BROWSER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let read = async {
        let mut bytes = Vec::new();
        stdout
            .take(STATUS_OUTPUT_LIMIT)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        child.wait().await.ok()?;
        Some(bytes)
    };
    let bytes = tokio::time::timeout(STATUS_TIMEOUT, read).await.ok()??;
    parse_login_status(&bytes)
}

/// `isAuthenticated` from `agent status --format json` (a pretty-printed
/// object, possibly after other output).
pub(crate) fn parse_login_status(output: &[u8]) -> Option<bool> {
    let start = output.iter().position(|byte| *byte == b'{')?;
    serde_json::from_slice::<Value>(&output[start..])
        .ok()?
        .get("isAuthenticated")?
        .as_bool()
}
