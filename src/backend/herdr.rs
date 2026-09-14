fn herdr_owns_prompt_submission(version: Option<&str>) -> bool {
    super::model::version_at_least(version, (0, 9, 0))
}

#[cfg(test)]
mod collaboration_submission_tests {
    use super::*;

    #[test]
    fn modern_herdr_never_gets_the_legacy_extra_enter() {
        for version in ["0.9.0", "v0.9.1", "0.10.0", "1.0.0", "0.9.0+build"] {
            assert!(herdr_owns_prompt_submission(Some(version)), "{version}");
        }
        for version in [
            None,
            Some("0.8.9"),
            Some("0.9.0-rc.1"),
            Some("0.9"),
            Some("unknown"),
        ] {
            assert!(!herdr_owns_prompt_submission(version), "{version:?}");
        }
    }
}

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(unix)]
use tokio::net::UnixStream;

use super::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, BoundInterrupt, BoundInterruptReceipt, BoundPrompt,
    BoundPromptReceipt, CreateTab, CreateWorkspace, OutputFormat, OutputSource, Pane, PaneId,
    PaneOutput, ReadPane, SendTextMode, SplitDirection, SplitPane, StartAgent, StartedAgent, Tab,
    TabId, TerminalBackend, Workspace, WorkspaceId, Worktree, WorktreePlacement, WorktreeRequest,
};

/// How long one herdr request may take, end to end.
///
/// A Unix socket read has no timeout of its own. So a herdr that accepts the
/// connection and then never answers -- wedged, deadlocked, stopped in a
/// debugger, or simply in the middle of something it will not come back from
/// -- held the task and the file descriptor forever, and every request that
/// landed on it did the same. The gateway leaked a task and an fd per request
/// for as long as the condition lasted, and the phone got a spinner that never
/// resolved instead of an error it could retry.
///
/// Deliberately generous: this is not a latency budget, it is the line between
/// "slow" and "never". Herdr answers these calls out of its own memory in
/// milliseconds, so a request still outstanding at thirty seconds is not
/// coming, and a bound that tried to be tight would start failing real calls
/// on a loaded machine -- which is worse than the leak.
///
/// Bounds `request_transport` only. See `activity_stream`, which must not have
/// one and says why.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

fn startup_refusal(code: &str, message: &str) -> BackendError {
    BackendError::Refused {
        code: Some(code.to_owned()),
        message: message.to_owned(),
    }
}

fn can_retry_agent_start(error: &BackendError) -> bool {
    // Herdr returns this before writing any input. A newly created shell may
    // still be running its startup scripts; socket failures are never retried.
    matches!(error, BackendError::Refused { code: Some(code), .. } if code == "agent_pane_busy")
}

fn startup_ready(
    agent: &Value,
    name: &str,
    terminal_id: &str,
    kind: &str,
) -> Result<bool, BackendError> {
    if agent["terminal_id"].as_str() != Some(terminal_id) || agent["name"].as_str() != Some(name) {
        return Err(startup_refusal(
            "agent_name_lost",
            "the assistant's terminal occupant changed during startup",
        ));
    }
    if agent["agent"].as_str().is_some_and(|actual| actual != kind) {
        return Err(startup_refusal(
            "agent_kind_mismatch",
            "a different agent appeared during startup",
        ));
    }
    match agent["agent_status"].as_str() {
        Some("blocked") => Err(startup_refusal(
            "agent_not_ready",
            "assistant needs attention in its terminal before receiving a task",
        )),
        Some("idle" | "done") if agent["interactive_ready"].as_bool() == Some(true) => Ok(true),
        Some("idle" | "done") if agent["launch_pending"].as_bool() != Some(true) => {
            Err(startup_refusal(
                "agent_start_failed",
                "assistant exited before becoming interactive",
            ))
        }
        _ => Ok(false),
    }
}

pub struct HerdrBackend {
    socket_path: PathBuf,
    /// Overridable so a test can prove the timeout fires without waiting out
    /// the real one.
    request_timeout: Duration,
    /// Whether this herdr answers `pane.process_info`. Assumed until the
    /// first refusal, then remembered: a herdr below the method's protocol
    /// is asked once per pane listing, not once per pane per listing, and
    /// its panes come back exactly as they did before the call existed.
    process_info: AtomicBool,
    /// The controlling terminal of each pane's shell, by shell pid -- the
    /// device it was found to be and the path it was found at. Resolving one
    /// is a scan of `/dev`; asking whether the cached answer still holds is
    /// one syscall, which is what every listing pays instead.
    terminals: Mutex<HashMap<u32, (u64, PathBuf)>>,
}

impl HerdrBackend {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self::with_request_timeout(socket_path, REQUEST_TIMEOUT)
    }

    fn with_request_timeout(socket_path: impl Into<PathBuf>, request_timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            request_timeout,
            process_info: AtomicBool::new(true),
            terminals: Mutex::new(HashMap::new()),
        }
    }

    async fn wait_bound_ready(&self, launch_id: &str) -> Result<(), BackendError> {
        tokio::time::timeout(self.request_timeout, async {
            loop {
                let response = self
                    .request(
                        "agent.get_bound",
                        json!({ "expected_launch_id": launch_id }),
                    )
                    .await
                    .map_err(bound_error)?;
                if bound_agent_ready(&response, launch_id)? {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .map_err(|_| {
            startup_refusal(
                "agent_not_ready",
                "assistant did not become ready before the deadline",
            )
        })?
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        let response = self
            .request_transport(method, params)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if let Some(error) = response.get("error") {
            return Err(BackendError::Refused {
                code: error.get("code").map(|code| match code {
                    Value::String(code) => code.clone(),
                    other => other.to_string(),
                }),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("request refused")
                    .to_owned(),
            });
        }
        Ok(response)
    }

    async fn request_transport(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        #[cfg(unix)]
        {
            // The whole exchange is inside the bound, not just the read: a
            // connect can hang too, on a socket whose listener is alive but
            // no longer accepting. Elapsing drops the future, which closes
            // the stream and releases the descriptor -- the point of the
            // bound is as much the fd as the task.
            tokio::time::timeout(self.request_timeout, self.exchange(method, params))
                .await
                .with_context(|| {
                    format!(
                        "herdr did not answer {method} within {}s",
                        self.request_timeout.as_secs()
                    )
                })?
        }

        #[cfg(not(unix))]
        {
            let _ = (method, params);
            anyhow::bail!("Herdr socket transport is unavailable on this platform")
        }
    }

    /// One request written and one response read back, with no bound of its
    /// own -- `request_transport` is the only caller and it supplies one.
    #[cfg(unix)]
    async fn exchange(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("failed to connect {}", self.socket_path.display()))?;
        let request = json!({
            "id": format!("gateway:{}", uuid::Uuid::new_v4()),
            "method": method,
            "params": params,
        });
        stream.write_all(request.to_string().as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok(serde_json::from_str(&line)?)
    }
}

/// Legacy collaboration eligibility belongs to the adapter interpreting ping.
/// This does not advertise atomic instance-bound prompt delivery.
fn collaboration_capabilities(version: Option<&str>) -> Vec<&'static str> {
    if super::model::version_at_least(version, (0, 9, 0)) {
        vec!["agent_collaboration"]
    } else {
        Vec::new()
    }
}

fn metadata_capabilities(response: &Value) -> Vec<&'static str> {
    let mut capabilities =
        collaboration_capabilities(response.pointer("/result/version").and_then(Value::as_str));
    // Interruption is independent of prompt readiness and screen observation.
    let native = &response["result"]["capabilities"];
    if native["agent_interrupt_bound"].as_bool() == Some(true)
        && native["agent_lifecycle_bound"].as_bool() == Some(true)
        && bound_identity(&native["owner_epoch"]).is_ok()
    {
        capabilities.push("instance_bound_interrupt");
    }
    // Managed delivery needs identity-scoped readiness and a snapshot for the
    // shared approval detector as well as the corresponding mutation method.
    if response
        .pointer("/result/capabilities/agent_get_bound")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return capabilities;
    }
    for (native, capability) in [
        ("agent_start_bound", "instance_bound_start"),
        ("agent_prompt_bound", "instance_bound_prompt"),
        ("agent_lifecycle_bound", "instance_bound_lifecycle"),
    ] {
        if response
            .pointer("/result/capabilities")
            .and_then(|value| value.get(native))
            .and_then(Value::as_bool)
            == Some(true)
        {
            if native == "agent_lifecycle_bound"
                && bound_identity(&response["result"]["capabilities"]["owner_epoch"]).is_err()
            {
                continue;
            }
            capabilities.push(capability);
        }
    }
    if native["agent_reporting_mcp_codex"].as_bool() == Some(true)
        && capabilities.contains(&"instance_bound_start")
        && capabilities.contains(&"instance_bound_lifecycle")
    {
        capabilities.push("reporting_mcp_codex");
    }
    capabilities
}

fn metadata_bound_agent_kinds(response: &Value) -> Option<Vec<String>> {
    if !metadata_capabilities(response).contains(&"instance_bound_start") {
        return None;
    }
    let values = response
        .pointer("/result/capabilities/bound_agent_kinds")?
        .as_array()?;
    if values.len() > 64 {
        return None;
    }
    let mut kinds = Vec::with_capacity(values.len());
    for value in values {
        let kind = value.as_str()?;
        if kind.is_empty()
            || kind.len() > 64
            || !kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
            || kinds.iter().any(|item| item == kind)
        {
            return None;
        }
        kinds.push(kind.to_owned());
    }
    Some(kinds)
}

fn bound_agent_ready(response: &Value, expected_launch_id: &str) -> Result<bool, BackendError> {
    let result = &response["result"];
    if result["type"] != "agent_bound_info" {
        return Err(BackendError::InvalidResponse("bound agent status"));
    }
    if result["launch_id"].as_str() != Some(expected_launch_id) {
        return Err(startup_refusal(
            "instance_changed",
            "assistant launch has changed",
        ));
    }
    let visible = result["visible_text"]
        .as_str()
        .filter(|text| text.len() <= 512 * 1024)
        .ok_or(BackendError::InvalidResponse(
            "bound agent visible snapshot",
        ))?;
    let agent = &result["agent"];
    if agent["agent_status"] == "blocked" || crate::approvals::detect(visible).is_some() {
        return Err(startup_refusal(
            "agent_blocked",
            "assistant requires interactive approval before receiving a prompt",
        ));
    }
    Ok(
        matches!(agent["agent_status"].as_str(), Some("idle" | "done"))
            && agent["interactive_ready"].as_bool() == Some(true)
            && agent["launch_pending"].as_bool() != Some(true),
    )
}

fn bound_error(error: BackendError) -> BackendError {
    match error {
        // Only documented zero-write refusals may invite a later explicit
        // attempt. Unknown failures and partial writes remain unconfirmed.
        BackendError::Refused {
            code: Some(code),
            message,
        } if matches!(
            code.as_str(),
            "instance_changed" | "agent_not_ready" | "agent_blocked" | "unsupported_agent_kind"
        ) =>
        {
            BackendError::Refused {
                code: Some(code),
                message,
            }
        }
        BackendError::Refused { .. } => BackendError::Unavailable,
        other => other,
    }
}

fn bound_identity(value: &Value) -> Result<&str, BackendError> {
    value
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .ok_or(BackendError::InvalidResponse("bound agent identity"))
}
fn lifecycle_receipt(
    response: &Value,
    launch_id: &str,
    owner_epoch: &str,
) -> Result<super::BoundLifecycle, BackendError> {
    let result = &response["result"];
    if result["type"] != "agent_bound_lifecycle"
        || result["launch_id"] != launch_id
        || result["owner_epoch"] != owner_epoch
    {
        return Ok(super::BoundLifecycle::Unknown);
    }
    match result["state"].as_str() {
        Some("live") => Ok(super::BoundLifecycle::Live),
        Some("exited") => Ok(super::BoundLifecycle::Exited {
            receipt_id: bound_identity(&result["receipt_id"])?.into(),
        }),
        _ => Ok(super::BoundLifecycle::Unknown),
    }
}

fn started_bound_agent(
    response: &Value,
    request: &StartAgent,
    operation_id: &str,
) -> Result<StartedAgent, BackendError> {
    let result = &response["result"];
    if result["type"] != "agent_started_bound" || result["operation_id"] != operation_id {
        return Err(BackendError::InvalidResponse(
            "bound agent start acknowledgement",
        ));
    }
    let launch_id = bound_identity(&result["launch_id"])?;
    let agent = agent_from_json(&result["agent"])?;
    // Direct creation precedes runtime agent detection. The immutable receipt
    // declares the requested profile independently of observed pane metadata.
    let declared_kind = result.get("agent_kind").map(bound_identity).transpose()?;
    let profile_matches = match declared_kind {
        Some(kind) => kind == request.kind,
        None => {
            result.get("owner_epoch").is_none()
                && agent.kind.as_deref() == Some(request.kind.as_str())
        }
    };
    if agent.pane_id != request.pane_id
        || !profile_matches
        || agent
            .kind
            .as_deref()
            .is_some_and(|kind| kind != request.kind)
    {
        return Err(BackendError::InvalidResponse(
            "bound agent start destination",
        ));
    }
    let argv = result["argv"]
        .as_array()
        .filter(|argv| !argv.is_empty())
        .ok_or(BackendError::InvalidResponse("bound agent argv"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(BackendError::InvalidResponse("bound agent argv"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected_argv = vec![request
        .executable
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| request.command.clone())];
    expected_argv.extend(request.args.iter().cloned());
    if argv != expected_argv {
        return Err(BackendError::InvalidResponse(
            "bound agent launched command",
        ));
    }
    Ok(StartedAgent {
        owner_epoch: result
            .get("owner_epoch")
            .map(bound_identity)
            .transpose()?
            .map(str::to_owned),
        launch_id: Some(launch_id.to_owned()),
        target: Some(agent.target),
        instance_id: agent.instance_id,
        argv: Some(argv),
    })
}

fn bound_prompt_receipt(
    response: &Value,
    request: &BoundPrompt,
) -> Result<BoundPromptReceipt, BackendError> {
    let result = &response["result"];
    if result["type"] != "agent_prompted_bound"
        || result["operation_id"] != request.operation_id
        || result["launch_id"] != request.expected_launch_id
        || result["input_disposition"] != "written"
        || result["bytes_written"].as_u64() != Some(request.text.len() as u64)
    {
        return Err(BackendError::InvalidResponse(
            "bound prompt acknowledgement",
        ));
    }
    Ok(BoundPromptReceipt {
        operation_id: request.operation_id.clone(),
        launch_id: request.expected_launch_id.clone(),
    })
}

fn interrupt_error(error: BackendError) -> BackendError {
    match error {
        BackendError::Refused {
            code: Some(code),
            message,
        } if matches!(
            code.as_str(),
            "instance_changed"
                | "unsupported"
                | "invalid_request"
                | "request_key_conflict"
                | "resource_limit"
                | "agent_not_ready"
        ) =>
        {
            BackendError::Refused {
                code: Some(code),
                message,
            }
        }
        BackendError::Refused { .. } => BackendError::Unavailable,
        other => other,
    }
}

fn bound_interrupt_receipt(
    response: &Value,
    request: &BoundInterrupt,
) -> Result<BoundInterruptReceipt, BackendError> {
    let result = &response["result"];
    if result["type"] != "agent_interrupted_bound"
        || result["operation_id"] != request.operation_id
        || result["launch_id"] != request.expected_launch_id
        || result["owner_epoch"] != request.expected_owner_epoch
        || result["key"] != "Escape"
        || result["bytes_written"].as_u64() != Some(1)
        || result["input_disposition"] != "written"
    {
        return Err(BackendError::InvalidResponse(
            "bound interruption acknowledgement",
        ));
    }
    Ok(BoundInterruptReceipt {
        operation_id: request.operation_id.clone(),
        launch_id: request.expected_launch_id.clone(),
        owner_epoch: request.expected_owner_epoch.clone(),
        receipt_id: bound_identity(&result["receipt_id"])?.into(),
        key: "Escape".into(),
        bytes_written: 1,
        input_disposition: "written".into(),
    })
}

impl TerminalBackend for HerdrBackend {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        Box::pin(async move {
            let response = self.request("ping", json!({})).await?;
            Ok(BackendMetadata {
                bound_agent_kinds: metadata_bound_agent_kinds(&response),
                kind: BackendKind::Herdr,
                capabilities: metadata_capabilities(&response),
                version: response
                    .pointer("/result/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                protocol: response.pointer("/result/protocol").and_then(Value::as_u64),
                compatibility_response: Some(response),
            })
        })
    }

    /// Deliberately has no read timeout, and must not be given one.
    ///
    /// `request_transport` is bounded by `REQUEST_TIMEOUT` because a request
    /// that has not been answered in thirty seconds is never going to be. This
    /// is the opposite shape: a subscription that is *supposed* to sit idle.
    /// A terminal nobody is typing into produces no events for hours, and that
    /// silence is the healthy state, not a symptom -- so any read bound here
    /// would tear down a working stream on a quiet session and reconnect it
    /// forever.
    ///
    /// The setup calls are still bounded: `list_panes` below goes through
    /// `request_transport`, so a herdr that will not answer cannot hang this
    /// function either. Only the `read_line` loop is exempt, and only because
    /// waiting is what it is for. A stream that has genuinely died surfaces as
    /// `Ok(0)` or a read error, both of which end it.
    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream> {
        Box::pin(async move {
            #[cfg(unix)]
            {
                let panes = self.list_panes().await?;
                let mut stream = UnixStream::connect(&self.socket_path)
                    .await
                    .map_err(|_| BackendError::Unavailable)?;
                let request = json!({
                    "id": format!("gateway:{}", uuid::Uuid::new_v4()),
                    "method": "events.subscribe",
                    "params": { "subscriptions": activity_subscriptions(&panes) }
                });
                stream
                    .write_all(request.to_string().as_bytes())
                    .await
                    .map_err(|_| BackendError::Unavailable)?;
                stream
                    .write_all(b"\n")
                    .await
                    .map_err(|_| BackendError::Unavailable)?;
                stream
                    .flush()
                    .await
                    .map_err(|_| BackendError::Unavailable)?;
                let activity = async_stream::stream! {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => break,
                            Ok(_) => match serde_json::from_str::<Value>(line.trim()) {
                                Ok(payload) => {
                                    let name = payload
                                        .get("event")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .replace('.', "_");
                                    yield Ok(BackendActivity { name, payload });
                                }
                                Err(_) => yield Err(BackendError::InvalidResponse("activity event")),
                            },
                            Err(_) => {
                                yield Err(BackendError::Unavailable);
                                break;
                            }
                        }
                    }
                };
                Ok(Box::pin(activity) as BackendActivityStream)
            }

            #[cfg(not(unix))]
            {
                Err(BackendError::Unavailable)
            }
        })
    }

    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>> {
        Box::pin(async move {
            let response = self.request("workspace.list", json!({})).await?;
            response
                .pointer("/result/workspaces")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("workspace list"))?
                .iter()
                .map(workspace_from_json)
                .collect()
        })
    }

    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>> {
        Box::pin(async move {
            let response = self.request("tab.list", json!({})).await?;
            response
                .pointer("/result/tabs")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("tab list"))?
                .iter()
                .map(tab_from_json)
                .collect()
        })
    }

    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>> {
        Box::pin(async move {
            let response = self.request("pane.list", json!({})).await?;
            let panes = response
                .pointer("/result/panes")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("pane list"))?
                .iter()
                .map(pane_from_json)
                .collect::<Result<Vec<Pane>, BackendError>>()?;
            // One `pane.process_info` per pane, all in flight at once: the
            // approval watcher lists every 1.5s, and N round trips of 0.1ms
            // each are cheap in parallel and still cheap in series, but there
            // is no reason to pay the series.
            Ok(
                futures::future::join_all(panes.into_iter().map(|pane| self.complete_pane(pane)))
                    .await,
            )
        })
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>> {
        Box::pin(async move {
            let response = self.request("agent.list", json!({})).await?;
            response
                .pointer("/result/agents")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("agent list"))?
                .iter()
                .map(agent_from_json)
                .collect()
        })
    }

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let response = self
                .request("pane.get", json!({ "pane_id": id.as_str() }))
                .await?;
            let pane = response.pointer("/result/pane").unwrap_or(&response);
            Ok(self.complete_pane(pane_from_json(pane)?).await)
        })
    }

    fn get_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, Agent> {
        Box::pin(async move {
            let response = self
                .request("agent.get", json!({ "target": target }))
                .await?;
            let agent = response.pointer("/result/agent").unwrap_or(&response);
            agent_from_json(agent)
        })
    }

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            let response = self
                .request(
                    "pane.read",
                    json!({
                        "pane_id": request.pane_id.as_str(),
                        "source": match request.source {
                            OutputSource::Visible => "visible",
                            OutputSource::Recent => "recent",
                            OutputSource::RecentUnwrapped => "recent_unwrapped",
                            OutputSource::Detection => "detection",
                        },
                        "format": match request.format {
                            OutputFormat::Text => "text",
                            OutputFormat::Ansi => "ansi",
                        },
                        "lines": request.lines,
                    }),
                )
                .await?;
            pane_output_from_response(&response)
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            if let Some(cwd) = &request.cwd {
                params.insert("cwd".into(), json!(cwd));
            }
            if let Some(label) = &request.label {
                params.insert("label".into(), json!(label));
            }
            params.insert("focus".into(), json!(request.focus));
            let response = self
                .request("workspace.create", Value::Object(params))
                .await?;
            workspace_from_json(
                response
                    .pointer("/result/workspace")
                    .ok_or(BackendError::InvalidResponse("created workspace"))?,
            )
        })
    }

    fn focus_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        self.command("workspace.focus", json!({ "workspace_id": id.as_str() }))
    }

    fn rename_workspace<'a>(
        &'a self,
        id: &'a WorkspaceId,
        label: &'a str,
    ) -> BackendFuture<'a, ()> {
        self.command(
            "workspace.rename",
            json!({ "workspace_id": id.as_str(), "label": label }),
        )
    }

    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        self.command("workspace.close", json!({ "workspace_id": id.as_str() }))
    }

    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab> {
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            if let Some(workspace_id) = &request.workspace_id {
                params.insert("workspace_id".into(), json!(workspace_id.as_str()));
            }
            if let Some(cwd) = &request.cwd {
                params.insert("cwd".into(), json!(cwd));
            }
            if let Some(label) = &request.label {
                params.insert("label".into(), json!(label));
            }
            params.insert("focus".into(), json!(request.focus));
            let response = self.request("tab.create", Value::Object(params)).await?;
            tab_from_json(
                response
                    .pointer("/result/tab")
                    .ok_or(BackendError::InvalidResponse("created tab"))?,
            )
        })
    }

    fn focus_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        self.command("tab.focus", json!({ "tab_id": id.as_str() }))
    }

    fn rename_tab<'a>(&'a self, id: &'a TabId, label: &'a str) -> BackendFuture<'a, ()> {
        self.command(
            "tab.rename",
            json!({ "tab_id": id.as_str(), "label": label }),
        )
    }

    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        self.command("tab.close", json!({ "tab_id": id.as_str() }))
    }

    fn focus_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        self.command("pane.focus", json!({ "pane_id": id.as_str() }))
    }

    fn rename_pane<'a>(&'a self, id: &'a PaneId, label: &'a str) -> BackendFuture<'a, ()> {
        self.command(
            "pane.rename",
            json!({ "pane_id": id.as_str(), "label": label }),
        )
    }

    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        self.command("pane.close", json!({ "pane_id": id.as_str() }))
    }

    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            params.insert("target_pane_id".into(), json!(request.pane_id.as_str()));
            params.insert(
                "direction".into(),
                json!(match request.direction {
                    SplitDirection::Right => "right",
                    SplitDirection::Down => "down",
                }),
            );
            if let Some(ratio) = request.ratio {
                params.insert("ratio".into(), json!(ratio));
            }
            if let Some(cwd) = &request.cwd {
                params.insert("cwd".into(), json!(cwd));
            }
            if let Some(env) = &request.env {
                params.insert("env".into(), json!(env));
            }
            let response = self.request("pane.split", Value::Object(params)).await?;
            let pane = response
                .pointer("/result/pane")
                .or_else(|| response.pointer("/result/root_pane"))
                .ok_or(BackendError::InvalidResponse("split pane"))?;
            Ok(self.complete_pane(pane_from_json(pane)?).await)
        })
    }

    /// herdr types; it does not paste. Both modes take the same call, and that
    /// is not an oversight.
    ///
    /// Measured against a live herdr (protocol 20) by recording the pane's own
    /// tty, with the recorder holding bracketed-paste mode on so markers would
    /// have shown if there were any:
    ///
    /// | call | delivered |
    /// |------|-----------|
    /// | `pane.send_text` with `abc`      | `abc` |
    /// | `pane.send_text` with `one\ntwo` | `one\ntwo` |
    /// | `pane.send_input` with `xyz`     | `ESC[200~xyz ESC[201~` |
    ///
    /// So `pane.send_text` is already the keystroke path, which is why the bug
    /// this card fixes -- an editor key row that types into the buffer instead
    /// of driving the editor -- was tmux-only. Sending `Keys` here needs no
    /// change and gets one anyway in the shape of this note, because the next
    /// person to read it will assume a method called `send_text` pastes.
    ///
    /// `Paste` deliberately keeps the same call rather than moving to
    /// `pane.send_input`. Two reasons, and the first is the ordinary one: this
    /// is what every herdr client gets today, and a patch release fixing a
    /// keystroke bug is not where a paste changes shape. The second is that
    /// `pane.send_input` is not in the protocol floor this gateway supports
    /// (`HERDR_PROTOCOL_MIN`, 17) as far as anything here can establish, and a
    /// call an older herdr rejects would turn a working composer into an
    /// error.
    ///
    /// It does mean `Paste` on herdr is not bracketed, so a multi-line
    /// composer message can submit at its first newline and an attachment path
    /// does not become an `[Image #N]` reference -- both of which `-p` gives
    /// the tmux backend. That is a real gap, it predates this card, and it
    /// wants its own change: `pane.send_input` gates its markers on the
    /// program's own DECSET 2004 exactly as tmux's `-p` does (verified: into a
    /// pane that never asked, it delivered a bare `xyz`), so it is the right
    /// call to move to once the protocol floor is settled.
    fn send_text<'a>(
        &'a self,
        id: &'a PaneId,
        text: &'a str,
        _mode: SendTextMode,
    ) -> BackendFuture<'a, ()> {
        self.command(
            "pane.send_text",
            json!({ "pane_id": id.as_str(), "text": text }),
        )
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
        // Herdr parses key names after trimming whitespace, so a literal
        // space is rejected. Keep the neutral API literal and translate only
        // at this boundary, preserving order and repeated spaces in one call.
        let keys: Vec<&str> = keys
            .iter()
            .map(|key| if key == " " { "space" } else { key.as_str() })
            .collect();
        self.command(
            "pane.send_keys",
            json!({ "pane_id": id.as_str(), "keys": keys }),
        )
    }

    fn focus_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, ()> {
        self.command("agent.focus", json!({ "target": target }))
    }

    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()> {
        self.command("agent.prompt", json!({ "target": target, "text": text }))
    }

    fn start_bound_agent<'a>(
        &'a self,
        request: &'a StartAgent,
        operation_id: &'a str,
        work_context_file: Option<&'a str>,
        reporting_mcp: Option<&'a super::ReportingMcp>,
    ) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let discovery = self.request("ping", json!({})).await?;
            if !metadata_capabilities(&discovery).contains(&"instance_bound_start") {
                return Err(BackendError::Unsupported("instance_bound_start"));
            }
            if reporting_mcp.is_some()
                && (request.kind != "codex"
                    || work_context_file.is_none()
                    || !metadata_capabilities(&discovery).contains(&"reporting_mcp_codex"))
            {
                return Err(BackendError::Unsupported("reporting_mcp_codex"));
            }
            let expected_epoch = if discovery
                .pointer("/result/capabilities/agent_lifecycle_bound")
                .and_then(Value::as_bool)
                == Some(true)
            {
                Some(
                    bound_identity(&discovery["result"]["capabilities"]["owner_epoch"])?.to_owned(),
                )
            } else {
                None
            };
            let response = self.request_transport("agent.start_bound", json!({
                "operation_id": operation_id, "pane_id": request.pane_id.as_str(), "kind": request.kind,
                "command": request.executable.as_ref().map(|path| path.to_string_lossy().into_owned()).unwrap_or_else(|| request.command.clone()),
                "args": request.args, "timeout_ms": request.timeout_ms,
                "work_context_file": work_context_file,
                "reporting_mcp": reporting_mcp,
            })).await.map_err(|_| BackendError::Unavailable)?;
            if response.get("error").is_some() {
                if let Some(epoch) = expected_epoch.as_deref() {
                    let receipt = &response["start_receipt"];
                    if receipt["effect"] == "not_started"
                        && receipt["operation_id"] == operation_id
                        && receipt["owner_epoch"] == epoch
                    {
                        return Err(BackendError::StartNotStarted {
                            code: response["error"]["code"]
                                .as_str()
                                .filter(|code| code.len() <= 128)
                                .map(str::to_owned),
                            operation_id: operation_id.into(),
                            owner_epoch: epoch.into(),
                            receipt_id: bound_identity(&receipt["receipt_id"])?.into(),
                        });
                    }
                }
                // Generic refusal after claiming native dispatch proves no lifecycle fact.
                return Err(BackendError::InvalidResponse(
                    "unproven native startup refusal",
                ));
            }
            if reporting_mcp.is_some() && response["result"]["reporting_mcp"] != "codex_stdio_v1" {
                return Err(BackendError::InvalidResponse(
                    "native reporting configuration receipt",
                ));
            }
            let started = started_bound_agent(&response, request, operation_id)?;
            if expected_epoch.is_some() && started.owner_epoch != expected_epoch {
                return Err(BackendError::InvalidResponse("native startup owner epoch"));
            }
            Ok(started)
        })
    }
    fn lifecycle_bound<'a>(
        &'a self,
        launch_id: &'a str,
        owner_epoch: &'a str,
    ) -> BackendFuture<'a, super::BoundLifecycle> {
        Box::pin(async move {
            if !self
                .metadata()
                .await?
                .capabilities
                .contains(&"instance_bound_lifecycle")
            {
                return Ok(super::BoundLifecycle::Unknown);
            }
            let response = self
                .request(
                    "agent.lifecycle_bound",
                    json!({"expected_launch_id":launch_id,"expected_owner_epoch":owner_epoch}),
                )
                .await?;
            lifecycle_receipt(&response, launch_id, owner_epoch)
        })
    }

    fn interrupt_bound_agent<'a>(
        &'a self,
        request: &'a BoundInterrupt,
    ) -> BackendFuture<'a, BoundInterruptReceipt> {
        Box::pin(async move {
            if [
                &request.operation_id,
                &request.expected_launch_id,
                &request.expected_owner_epoch,
            ]
            .iter()
            .any(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
            {
                return Err(startup_refusal(
                    "invalid_request",
                    "invalid bound interruption identity",
                ));
            }
            if !self
                .metadata()
                .await?
                .capabilities
                .contains(&"instance_bound_interrupt")
            {
                return Err(BackendError::Unsupported("instance_bound_interrupt"));
            }
            // No idle wait, mutable pane lookup, key fallback or retry: native
            // admission owns the exact epoch/launch comparison and one write.
            let response = self
                .request(
                    "agent.interrupt_bound",
                    json!({
                        "operation_id": request.operation_id,
                        "expected_launch_id": request.expected_launch_id,
                        "expected_owner_epoch": request.expected_owner_epoch,
                    }),
                )
                .await
                .map_err(interrupt_error)?;
            bound_interrupt_receipt(&response, request)
        })
    }

    fn prompt_bound_agent<'a>(
        &'a self,
        request: &'a BoundPrompt,
    ) -> BackendFuture<'a, BoundPromptReceipt> {
        Box::pin(async move {
            if !self
                .metadata()
                .await?
                .capabilities
                .contains(&"instance_bound_prompt")
            {
                return Err(BackendError::Unsupported("instance_bound_prompt"));
            }
            self.wait_bound_ready(&request.expected_launch_id).await?;
            let response = self.request("agent.prompt_bound", json!({
                "operation_id": request.operation_id, "expected_launch_id": request.expected_launch_id, "text": request.text,
            })).await.map_err(bound_error)?;
            bound_prompt_receipt(&response, request)
        })
    }
    fn preflight_bound_prompt<'a>(&'a self, launch_id: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            if !self
                .metadata()
                .await?
                .capabilities
                .contains(&"instance_bound_prompt")
            {
                return Err(BackendError::Unsupported("instance_bound_prompt"));
            }
            self.wait_bound_ready(launch_id).await
        })
    }

    fn needs_submit_keypress(&self) -> BackendFuture<'_, bool> {
        Box::pin(async move {
            let metadata = self.metadata().await?;
            Ok(!herdr_owns_prompt_submission(metadata.version.as_deref()))
        })
    }

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            // Names are unique among live agents, even for the same kind.
            let name = format!("muqun-{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
            params.insert("name".into(), json!(name));
            params.insert("kind".into(), json!(request.kind));
            params.insert("pane_id".into(), json!(request.pane_id.as_str()));
            params.insert("timeout_ms".into(), json!(request.timeout_ms));
            if !request.args.is_empty() {
                params.insert("args".into(), json!(request.args));
            }
            let shell_deadline =
                tokio::time::Instant::now() + Duration::from_millis(request.timeout_ms.min(3000));
            let response = loop {
                match self
                    .request("agent.start", Value::Object(params.clone()))
                    .await
                {
                    Err(error)
                        if can_retry_agent_start(&error)
                            && tokio::time::Instant::now() < shell_deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                    result => break result?,
                }
            };
            // The socket acknowledges launching; Herdr's CLI adds its own
            // readiness wait. Mirror that here, pinned to this occupant, so
            // callers never send a task into a startup or approval dialog.
            let instance_id = if let Some(terminal_id) = response
                .pointer("/result/agent/terminal_id")
                .and_then(Value::as_str)
            {
                tokio::time::timeout(Duration::from_millis(request.timeout_ms), async {
                    loop {
                        let current = self.request("agent.get", json!({ "target": name })).await?;
                        let agent = current
                            .pointer("/result/agent")
                            .ok_or(BackendError::InvalidResponse("agent startup"))?;
                        if startup_ready(agent, &name, terminal_id, &request.kind)? {
                            // The launch alias precedes optional session hooks.
                            if let Some(instance_id) = agent_instance_id(agent) {
                                return Ok::<_, BackendError>(Some(instance_id));
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                })
                .await
                .map_err(|_| {
                    startup_refusal(
                        "agent_start_timeout",
                        "assistant did not become ready before the startup deadline",
                    )
                })??
            } else {
                None
            };
            let argv = response
                .pointer("/result/argv")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                });
            Ok(StartedAgent {
                owner_epoch: None,
                launch_id: None,
                argv,
                instance_id,
                target: Some(name),
            })
        })
    }

    fn list_worktrees<'a>(&'a self, cwd: &'a PathBuf) -> BackendFuture<'a, Vec<Worktree>> {
        Box::pin(async move {
            let response = self.request("worktree.list", json!({ "cwd": cwd })).await?;
            response
                .pointer("/result/worktrees")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("worktree list"))?
                .iter()
                .map(|value| {
                    Ok(Worktree {
                        path: PathBuf::from(required_string(value, "path", "worktree")?),
                        branch: value
                            .get("branch")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
    }

    fn open_worktree<'a>(
        &'a self,
        request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async move {
            let response = self
                .request("worktree.open", worktree_params(request))
                .await?;
            worktree_placement_from_json(&response)
        })
    }

    fn create_worktree<'a>(
        &'a self,
        request: &'a WorktreeRequest,
    ) -> BackendFuture<'a, WorktreePlacement> {
        Box::pin(async move {
            let response = self
                .request("worktree.create", worktree_params(request))
                .await
                .map_err(worktree_error)?;
            worktree_placement_from_json(&response)
        })
    }
}

impl HerdrBackend {
    fn command<'a>(&'a self, method: &'a str, params: Value) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.request(method, params).await?;
            Ok(())
        })
    }

    /// The two facts a pane payload does not carry and `pane.process_info`
    /// does: what the pane is running, and how wide it is.
    ///
    /// herdr's pane payload names no foreground program -- its `PaneInfo`
    /// schema has `agent`, a title the program may or may not set, and
    /// nothing else -- so `foreground_command` was `None` for every herdr
    /// pane, and a client keyed on it (the app's editor detection, this
    /// gateway's own `ScrollbackStore::is_editor_command`) treated an nvim
    /// pane as a shell: read it unwrapped, folded it as scrollback, and drew a
    /// 32-column screen as thirteen 178-column lines. Measured live on herdr
    /// 0.8.2 (protocol 20): `pane.list` says nothing for the nvim pane, and
    /// `pane.process_info` for the same pane answers `foreground_processes:
    /// [{ argv0: "nvim", ... }]`, `shell_pid: 28305`, in 0.11ms.
    ///
    /// The width comes off the same answer, one step removed. herdr reports
    /// no columns, and the note on `pane_from_json` says why the layout rect
    /// cannot stand in for them (re-measured today: rects of 67, 34 and 33
    /// were grids of 64, 32 and 30). The shell's controlling terminal knows,
    /// though: it is the pty herdr sized, and `TIOCGWINSZ` on it is exactly
    /// the `stty size` the shell would print. See `pane_grid`.
    ///
    /// Total, in both directions: a refusal, an old herdr without the method,
    /// a pane whose shell has no terminal this process may open -- each leaves
    /// the pane as it was parsed, which is the pane every client got before
    /// this existed. herdr's own `foreground_command`, should it ever send
    /// one, is kept over the derived one.
    async fn complete_pane(&self, mut pane: Pane) -> Pane {
        if !self.process_info.load(Ordering::Relaxed) {
            return pane;
        }
        let info = match self
            .request("pane.process_info", json!({ "pane_id": pane.id.as_str() }))
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if unknown_method(&error) {
                    self.process_info.store(false, Ordering::Relaxed);
                }
                return pane;
            }
        };
        let info = info.pointer("/result/process_info").unwrap_or(&info);
        if pane.foreground_command.is_none() {
            pane.foreground_command = foreground_command_from_process_info(info);
        }
        let shell = info
            .get("shell_pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok());
        if let Some((columns, rows)) = shell.and_then(|pid| self.pane_grid(pid)) {
            if pane.width.is_none() {
                pane.width = Some(columns);
            }
            if pane.height.is_none() {
                pane.height = Some(rows);
            }
        }
        pane
    }

    /// The grid of the terminal `pid` is running on, columns then rows.
    ///
    /// Two steps. The pid's controlling terminal is a device number (macOS:
    /// `proc_pidinfo`; Linux: `/proc/<pid>/stat`), and the device number is
    /// found under `/dev` by scanning for the character device that carries
    /// it -- once, because the answer is cached against the device number
    /// and re-checked on every call: a pid the kernel has handed to a new
    /// process with a new terminal fails the check and is scanned again,
    /// rather than reported at the old terminal's width.
    ///
    /// The device is opened read-only with `O_NOCTTY | O_NONBLOCK`, asked
    /// its size, and closed. Nothing is read from it and it never becomes this
    /// process's terminal. It is a pty slave owned by the same user herdr and
    /// this gateway run as, which is the only reason the open succeeds.
    fn pane_grid(&self, pid: u32) -> Option<(u32, u32)> {
        let device = tty::controlling_terminal(pid)?;
        let path = {
            let mut terminals = self.terminals.lock().ok()?;
            match terminals.get(&pid) {
                Some((known, path)) if *known == device => path.clone(),
                _ => {
                    let path = tty::device_path(device)?;
                    terminals.insert(pid, (device, path.clone()));
                    path
                }
            }
        };
        let grid = tty::window_size(&path);
        if grid.is_none() {
            if let Ok(mut terminals) = self.terminals.lock() {
                terminals.remove(&pid);
            }
        }
        grid
    }
}

/// The program at the front of a pane, as tmux's `pane_current_command`
/// would name it, from a `pane.process_info` answer.
///
/// The process group leader is the one that owns the terminal, so it is the
/// one named when herdr lists several (`cat | less` is `less`, as tmux says);
/// the first listed stands in where the leader is not among them. `argv0`
/// over `name`: for Claude Code herdr's `name` is the node binary's own
/// (`2.1.263`, measured) while `argv0` is `claude`. A login shell announces
/// itself as `-zsh`; the dash is the shell's, not the program's, and tmux
/// strips it too.
fn foreground_command_from_process_info(info: &Value) -> Option<String> {
    let processes = info.get("foreground_processes")?.as_array()?;
    let leader = info
        .get("foreground_process_group_id")
        .and_then(Value::as_u64);
    let process = processes
        .iter()
        .find(|process| leader.is_some() && process.get("pid").and_then(Value::as_u64) == leader)
        .or_else(|| processes.first())?;
    let name = process
        .get("argv0")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| process.get("name").and_then(Value::as_str))?;
    let name = name.trim().trim_start_matches('-');
    let name = name.rsplit('/').next().unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

/// Whether herdr refused a call because it does not know the method -- a
/// herdr older than the method, which the protocol floor (`HERDR_PROTOCOL_MIN`)
/// admits. Shared by every optional call so they age the same way.
fn unknown_method(error: &BackendError) -> bool {
    match error {
        BackendError::Refused { code, message } => code.as_deref().is_some_and(|code| {
            matches!(code, "method_not_found" | "unknown_method" | "-32601")
                || (code == "invalid_request" && message.contains("unknown variant"))
        }),
        _ => false,
    }
}

/// A pty's size, asked of the pty itself.
#[cfg(unix)]
mod tty {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};
    use std::path::{Path, PathBuf};

    /// The device number of `pid`'s controlling terminal, or `None` for a
    /// process without one -- or one this process may not ask about.
    #[cfg(target_os = "macos")]
    pub fn controlling_terminal(pid: u32) -> Option<u64> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is exactly `proc_bsdinfo` and the kernel writes
        // at most `size` bytes into it; the result is read only when the
        // kernel says it filled the whole struct.
        let read = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if read != size {
            return None;
        }
        // SAFETY: `proc_pidinfo` returned the full struct size, so every field
        // has been written.
        let info = unsafe { info.assume_init() };
        (info.e_tdev != u32::MAX).then_some(u64::from(info.e_tdev))
    }

    #[cfg(target_os = "linux")]
    pub fn controlling_terminal(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name is parenthesised and may itself hold spaces and
        // parentheses; every field after it is fixed, so count from the last
        // closing parenthesis: state, ppid, pgrp, session, tty_nr.
        let rest = &stat[stat.rfind(')')? + 1..];
        let tty: u64 = rest.split_whitespace().nth(4)?.parse().ok()?;
        (tty != 0).then_some(tty)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    pub fn controlling_terminal(_pid: u32) -> Option<u64> {
        None
    }

    /// The character device under `/dev` carrying `device`. Linux keeps its
    /// ptys in `/dev/pts`; macOS names them `/dev/ttysNNN` in `/dev` itself.
    /// Both are looked in, the smaller first.
    pub fn device_path(device: u64) -> Option<PathBuf> {
        ["/dev/pts", "/dev"].into_iter().find_map(|directory| {
            std::fs::read_dir(directory).ok()?.find_map(|entry| {
                let entry = entry.ok()?;
                let metadata = entry.metadata().ok()?;
                (metadata.file_type().is_char_device() && metadata.rdev() == device)
                    .then(|| entry.path())
            })
        })
    }

    /// `TIOCGWINSZ` on the terminal at `path`: columns then rows, or `None`
    /// where it cannot be opened, is not a terminal, or has no size yet.
    pub fn window_size(path: &Path) -> Option<(u32, u32)> {
        let terminal = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(path)
            .ok()?;
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `TIOCGWINSZ` writes one `winsize` through the pointer it is
        // handed, and `size` is exactly that and outlives the call.
        let answered =
            unsafe { libc::ioctl(terminal.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) };
        (answered == 0 && size.ws_col > 0 && size.ws_row > 0)
            .then(|| (u32::from(size.ws_col), u32::from(size.ws_row)))
    }
}

#[cfg(not(unix))]
mod tty {
    use std::path::{Path, PathBuf};

    pub fn controlling_terminal(_pid: u32) -> Option<u64> {
        None
    }

    pub fn device_path(_device: u64) -> Option<PathBuf> {
        None
    }

    pub fn window_size(_path: &Path) -> Option<(u32, u32)> {
        None
    }
}

fn workspace_from_json(value: &Value) -> Result<Workspace, BackendError> {
    Ok(Workspace {
        id: WorkspaceId::new(required_string(value, "workspace_id", "workspace")?),
        number: value
            .get("number")
            .and_then(Value::as_u64)
            .and_then(|number| u32::try_from(number).ok()),
        label: value
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        focused: value
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        active_tab_id: value
            .get("active_tab_id")
            .and_then(Value::as_str)
            .map(TabId::new),
        tab_count: value
            .get("tab_count")
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok()),
        pane_count: value
            .get("pane_count")
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok()),
        agent_status: AgentStatus::parse(value.get("agent_status").and_then(Value::as_str)),
        repo_root: value
            .pointer("/worktree/repo_root")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        checkout_path: value
            .pointer("/worktree/checkout_path")
            .and_then(Value::as_str)
            .map(PathBuf::from),
    })
}

fn tab_from_json(value: &Value) -> Result<Tab, BackendError> {
    Ok(Tab {
        id: TabId::new(required_string(value, "tab_id", "tab")?),
        workspace_id: WorkspaceId::new(required_string(value, "workspace_id", "tab")?),
        label: value
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        focused: value
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        active_pane_id: value
            .get("active_pane_id")
            .and_then(Value::as_str)
            .map(PaneId::new),
        pane_count: value
            .get("pane_count")
            .and_then(Value::as_u64)
            .and_then(|count| u32::try_from(count).ok()),
    })
}

fn pane_from_json(value: &Value) -> Result<Pane, BackendError> {
    Ok(Pane {
        id: PaneId::new(required_string(value, "pane_id", "pane")?),
        terminal_id: value
            .get("terminal_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        workspace_id: WorkspaceId::new(required_string(value, "workspace_id", "pane")?),
        tab_id: TabId::new(required_string(value, "tab_id", "pane")?),
        label: value
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        terminal_title: value
            .get("terminal_title_stripped")
            .or_else(|| value.get("terminal_title"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: value
            .get("cwd")
            .or_else(|| value.get("foreground_cwd"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        focused: value
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        // herdr's pane payload has no `width`, and its `PaneInfo` schema has
        // never declared one; this read stays because a future herdr may add
        // it, and costs nothing until then. Nothing HERE derives one:
        //
        // - `pane.layout` does report a per-pane `rect`, but that is the box
        //   drawn on screen, not the grid the program sees. Measured live at
        //   protocol 20 against a split pane: `rect` was 47x39 while the shell
        //   inside it reported `stty size` = 37 rows by 44 columns. The
        //   vertical overhead is 2 and the horizontal 3, so there is no single
        //   border width to subtract -- and herdr has four independent knobs
        //   (`pane_borders`, `pane_outer_borders`, `pane_gaps`,
        //   `pane_scrollbars`) that move it. Re-measured on herdr 0.8.2 with
        //   three columns side by side: rects 67, 34 and 33 wide were grids of
        //   64, 32 and 30. Subtracting a guess would put the wrong column
        //   count on a real pane, which is worse than none.
        // - A read cannot say either: herdr trims each row, so the widest row
        //   of a `visible` read is the pane's width only when something
        //   happened to fill a row, and a quiet shell measures as wide as its
        //   prompt.
        //
        // The width is filled in afterwards by `complete_pane`, off the size
        // of the terminal the pane's shell is actually running on.
        width: value
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        // `scroll.viewport_rows` *is* the height, and it is already in this
        // payload. Measured live: a pane whose shell reported 37 rows came
        // back with `viewport_rows: 37`, while `pane.layout`'s rect said 39.
        // Reading `height` first keeps the field honest if herdr ever sends
        // one of its own.
        height: value
            .get("height")
            .and_then(Value::as_u64)
            .or_else(|| {
                value
                    .pointer("/scroll/viewport_rows")
                    .and_then(Value::as_u64)
            })
            .and_then(|v| u32::try_from(v).ok()),
        revision: value.get("revision").and_then(Value::as_u64),
        foreground_command: value
            .get("foreground_command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent: value
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_owned),
        agent_status: AgentStatus::parse(value.get("agent_status").and_then(Value::as_str)),
        max_offset_from_bottom: value
            .pointer("/scroll/max_offset_from_bottom")
            .and_then(Value::as_u64)
            .and_then(|offset| u32::try_from(offset).ok()),
        viewport_rows: value
            .pointer("/scroll/viewport_rows")
            .and_then(Value::as_u64)
            .and_then(|rows| u32::try_from(rows).ok())
            .or_else(|| {
                value
                    .get("height")
                    .and_then(Value::as_u64)
                    .and_then(|rows| u32::try_from(rows).ok())
            }),
        // Herdr's own envelope has never carried this; nothing to read, and
        // nothing to derive it from either -- its published schema at protocol
        // 20 has no alternate-screen flag on any pane shape, and the
        // `InputState` that does track one is handoff state rather than API
        // state. A reader that needs to know whether a pane's program owns the
        // screen has to fall back on `foreground_command`, which is what
        // `ScrollbackStore::is_editor_command` already does.
        alternate_on: None,
        // herdr's socket API exposes no cursor position at all: the only
        // `cursor` in its protocol-20 schema is the Cursor editor as an
        // integration target. Absence is part of the contract for these two
        // fields, so this is a supported answer rather than a gap.
        cursor_x: None,
        cursor_y: None,
    })
}

fn agent_instance_id(value: &Value) -> Option<String> {
    // A present bound identity must validate as a pair; malformed native
    // evidence never falls back to a reusable alias or conversation.
    if value.get("launch_id").is_some_and(|id| !id.is_null()) {
        bound_identity(&value["owner_epoch"]).ok()?;
        return bound_identity(&value["launch_id"]).ok().map(str::to_owned);
    }
    let terminal = value.get("terminal_id")?.as_str()?;
    if terminal.is_empty() {
        return None;
    }
    // Herdr clears aliases when their agent exits or is replaced. Gateway
    // launch aliases are fresh random tokens, never reused by a later launch.
    // Session hooks may arrive only after the first prompt; other supported
    // agents have no session integration. Neither changes the launch identity.
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        if name.strip_prefix("muqun-").is_some_and(|token| {
            token.len() == 20 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Some(json!([terminal, "launch", name]).to_string());
        }
    }
    let conversation = value
        .pointer("/agent_session/value")
        .and_then(Value::as_str)?;
    if terminal.is_empty() || conversation.is_empty() {
        return None;
    }
    Some(json!([terminal, conversation]).to_string())
}

fn agent_from_json(value: &Value) -> Result<Agent, BackendError> {
    let pane_id = required_string(value, "pane_id", "agent")?;
    let launch_id = if value.get("launch_id").is_some_and(|id| !id.is_null()) {
        bound_identity(&value["owner_epoch"])?;
        Some(bound_identity(&value["launch_id"])?.to_owned())
    } else {
        None
    };
    Ok(Agent {
        launch_id,
        instance_id: agent_instance_id(value),
        target: value
            .get("target")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(pane_id)
            .to_owned(),
        pane_id: PaneId::new(pane_id),
        workspace_id: value
            .get("workspace_id")
            .and_then(Value::as_str)
            .map(WorkspaceId::new),
        tab_id: value.get("tab_id").and_then(Value::as_str).map(TabId::new),
        kind: value
            .get("agent")
            .or_else(|| value.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        display_agent: value
            .get("display_agent")
            .and_then(Value::as_str)
            .map(str::to_owned),
        status: AgentStatus::parse(value.get("agent_status").and_then(Value::as_str)),
        state_change_seq: value.get("state_change_seq").and_then(Value::as_u64),
    })
}

fn worktree_params(request: &WorktreeRequest) -> Value {
    let mut params = serde_json::Map::new();
    params.insert("cwd".into(), json!(request.cwd));
    params.insert("branch".into(), json!(request.branch));
    params.insert("focus".into(), json!(request.focus));
    if let Some(label) = &request.label {
        params.insert("label".into(), json!(label));
    }
    Value::Object(params)
}

fn worktree_placement_from_json(response: &Value) -> Result<WorktreePlacement, BackendError> {
    Ok(WorktreePlacement {
        workspace_id: WorkspaceId::new(
            response
                .pointer("/result/workspace/workspace_id")
                .and_then(Value::as_str)
                .ok_or(BackendError::InvalidResponse("worktree workspace"))?,
        ),
        pane_id: PaneId::new(
            response
                .pointer("/result/root_pane/pane_id")
                .and_then(Value::as_str)
                .ok_or(BackendError::InvalidResponse("worktree pane"))?,
        ),
        path: response
            .pointer("/result/worktree/path")
            .and_then(Value::as_str)
            .map(PathBuf::from),
    })
}

fn worktree_error(error: BackendError) -> BackendError {
    if unknown_method(&error) {
        BackendError::Unsupported("worktrees")
    } else {
        error
    }
}

fn activity_subscriptions(panes: &[Pane]) -> Vec<Value> {
    let mut subscriptions = vec![
        json!({ "type": "workspace.created" }),
        json!({ "type": "workspace.updated" }),
        json!({ "type": "workspace.metadata_updated" }),
        json!({ "type": "workspace.renamed" }),
        json!({ "type": "workspace.moved" }),
        json!({ "type": "workspace.closed" }),
        json!({ "type": "workspace.focused" }),
        json!({ "type": "tab.created" }),
        json!({ "type": "tab.closed" }),
        json!({ "type": "tab.focused" }),
        json!({ "type": "tab.renamed" }),
        json!({ "type": "tab.moved" }),
        json!({ "type": "pane.created" }),
        json!({ "type": "pane.updated" }),
        json!({ "type": "pane.closed" }),
        json!({ "type": "pane.focused" }),
        json!({ "type": "pane.moved" }),
        json!({ "type": "pane.exited" }),
        json!({ "type": "pane.agent_detected" }),
        json!({ "type": "layout.updated" }),
        json!({ "type": "worktree.created" }),
        json!({ "type": "worktree.opened" }),
        json!({ "type": "worktree.removed" }),
    ];
    subscriptions.extend(
        panes.iter().map(
            |pane| json!({ "type": "pane.agent_status_changed", "pane_id": pane.id.as_str() }),
        ),
    );
    subscriptions
}

/// Turn a herdr `pane.read` text answer into a [`PaneOutput`].
///
/// `range` is always `None`. herdr's `pane.read` has no range parameter, so
/// it can never honour a requested `[start, end)` — any range it constructed
/// could only ever describe the tail it happened to serve, not the span the
/// caller asked for. A response cannot tell those two apart once `range` is
/// attached: `start == 0` reads identically whether it means "you asked for
/// the top" or "this is merely where our unrequested tail began", and the
/// latter is the false "top of scrollback reached" signal on exactly the
/// panes the gateway's own ring buffers for (`ScrollbackStore::keeps` keys
/// off the same `max_offset_from_bottom == 0` herdr reports here) — the ring
/// simultaneously advertises real depth for that pane in its entity
/// (`ScrollbackStore::amend`), so a fabricated `range` would have the read
/// and the entity disagree about whether there is more to pull.
///
/// Leaving it `None` unconditionally is what makes `range` mean one thing
/// across both backends: *the backend served the range you requested*.
/// Callers already have a measured fallback for herdr today (paging by
/// repeated `lines` reads), and `None` is what keeps them on it.
fn herdr_pane_output(text: &str) -> PaneOutput {
    PaneOutput {
        text: text.to_owned(),
        revision: None,
        range: None,
    }
}

/// Parse a herdr `pane.read` response into a [`PaneOutput`].
fn pane_output_from_response(response: &Value) -> Result<PaneOutput, BackendError> {
    let text = [
        "/result/read/output",
        "/result/read/text",
        "/result/output",
        "/result/text",
    ]
    .into_iter()
    .find_map(|pointer| response.pointer(pointer).and_then(Value::as_str))
    .ok_or(BackendError::InvalidResponse("pane output"))?;
    let revision = ["/result/read/revision", "/result/revision"]
        .into_iter()
        .find_map(|pointer| response.pointer(pointer).and_then(Value::as_u64));
    Ok(PaneOutput {
        revision,
        ..herdr_pane_output(text)
    })
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    context: &'static str,
) -> Result<&'a str, BackendError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or(BackendError::InvalidResponse(context))
}

#[cfg(test)]
mod tests {
    #[test]
    fn created_only_launch_receipt_preserves_identity_without_claiming_readiness() {
        let request = StartAgent {
            pane_id: PaneId::new("w2:p1"),
            kind: "codex".into(),
            command: "/fixture/codex".into(),
            executable: None,
            args: vec![],
            timeout_ms: 5000,
        };
        // Shape captured from the real native socket immediately after spawn;
        // declared agent_kind is the additive immutable receipt contract.
        let captured = json!({"result":{"type":"agent_started_bound","agent_kind":"codex","owner_epoch":"owner-fixture","operation_id":"fixture-start","launch_id":"launch-fixture","argv":["/fixture/codex"],"agent":{"terminal_id":"term-fixture","name":"bound-fixture","agent_status":"unknown","workspace_id":"w2","tab_id":"w2:t1","pane_id":"w2:p1","focused":true,"launch_pending":true,"state_change_seq":0,"cwd":"/fixture","foreground_cwd":"/fixture","revision":0}}});
        let started = started_bound_agent(&captured, &request, "fixture-start").unwrap();
        assert_eq!(started.launch_id.as_deref(), Some("launch-fixture"));
        assert_eq!(started.owner_epoch.as_deref(), Some("owner-fixture"));
        let readiness = json!({"result":{"type":"agent_bound_info","launch_id":"launch-fixture","visible_text":"","agent":captured["result"]["agent"]}});
        assert!(!bound_agent_ready(&readiness, "launch-fixture").unwrap());
        let mut missing = captured.clone();
        missing["result"]
            .as_object_mut()
            .unwrap()
            .remove("agent_kind");
        assert!(started_bound_agent(&missing, &request, "fixture-start").is_err());
        let mut wrong_declared = captured.clone();
        wrong_declared["result"]["agent_kind"] = json!("claude");
        assert!(started_bound_agent(&wrong_declared, &request, "fixture-start").is_err());
        let mut wrong_observed = captured;
        wrong_observed["result"]["agent"]["agent"] = json!("claude");
        assert!(started_bound_agent(&wrong_observed, &request, "fixture-start").is_err());
    }
    #[test]
    fn lifecycle_requires_exact_epoch_and_reaped_receipt() {
        let valid = serde_json::json!({"result":{"type":"agent_bound_lifecycle","launch_id":"generation","owner_epoch":"epoch","state":"exited","receipt_id":"reaped"}});
        assert_eq!(
            super::lifecycle_receipt(&valid, "generation", "epoch").unwrap(),
            crate::backend::BoundLifecycle::Exited {
                receipt_id: "reaped".into()
            }
        );
        assert_eq!(
            super::lifecycle_receipt(&valid, "generation", "old-epoch").unwrap(),
            crate::backend::BoundLifecycle::Unknown
        );
        assert_eq!(
            super::lifecycle_receipt(&valid, "other-generation", "epoch").unwrap(),
            crate::backend::BoundLifecycle::Unknown
        );
        let mut missing = valid.clone();
        missing["result"]
            .as_object_mut()
            .unwrap()
            .remove("receipt_id");
        assert!(super::lifecycle_receipt(&missing, "generation", "epoch").is_err());
        let mut unavailable = valid;
        unavailable["result"]["state"] = serde_json::json!("unknown");
        assert_eq!(
            super::lifecycle_receipt(&unavailable, "generation", "epoch").unwrap(),
            crate::backend::BoundLifecycle::Unknown
        );
    }
    #[test]
    fn lifecycle_capability_requires_current_owner_evidence() {
        let mut metadata = serde_json::json!({"result":{"capabilities":{"agent_get_bound":true,"agent_lifecycle_bound":true}}});
        assert!(!super::metadata_capabilities(&metadata).contains(&"instance_bound_lifecycle"));
        metadata["result"]["capabilities"]["owner_epoch"] = serde_json::json!("owner");
        assert!(super::metadata_capabilities(&metadata).contains(&"instance_bound_lifecycle"));
    }
    #[test]
    fn bound_capabilities_require_explicit_native_boolean_evidence() {
        for response in [
            json!({"result":{"version":"99.0.0"}}),
            json!({"result":{"capabilities":{"agent_start_bound":"true","agent_prompt_bound":1}}}),
            json!({"result":{"capabilities":{"agent_start_bound":false,"agent_prompt_bound":false}}}),
            json!({"result":{"capabilities":{"agent_start_bound":true,"agent_prompt_bound":true}}}),
        ] {
            let capabilities = super::metadata_capabilities(&response);
            assert!(!capabilities.contains(&"instance_bound_start"));
            assert!(!capabilities.contains(&"instance_bound_prompt"));
        }
        assert_eq!(
            super::metadata_capabilities(
                &json!({"result":{"capabilities":{"agent_start_bound":true,"agent_prompt_bound":true,"agent_get_bound":true}}})
            ),
            vec!["instance_bound_start", "instance_bound_prompt"]
        );
    }

    #[test]
    fn bound_prompt_acknowledgements_match_operation_generation_and_utf8_bytes() {
        let request = BoundPrompt {
            operation_id: "op".into(),
            expected_launch_id: "launch".into(),
            text: "你好".into(),
        };
        let response = json!({"result":{"type":"agent_prompted_bound","operation_id":"op","launch_id":"launch","input_disposition":"written","bytes_written":6}});
        assert_eq!(
            bound_prompt_receipt(&response, &request).unwrap().launch_id,
            "launch"
        );
        for (key, value) in [
            ("operation_id", json!("other")),
            ("launch_id", json!("replacement")),
            ("input_disposition", json!("partial")),
            ("bytes_written", json!(2)),
            ("type", json!("agent_prompted")),
        ] {
            let mut invalid = response.clone();
            invalid["result"][key] = value;
            assert!(matches!(
                bound_prompt_receipt(&invalid, &request),
                Err(BackendError::InvalidResponse(_))
            ));
        }
        assert!(matches!(
            bound_error(startup_refusal("delivery_unconfirmed", "partial input")),
            BackendError::Unavailable
        ));
        assert!(
            matches!(bound_error(startup_refusal("instance_changed","replaced")),BackendError::Refused {code:Some(code),..} if code=="instance_changed")
        );
        assert!(matches!(
            bound_error(startup_refusal("unknown_failure", "unknown outcome")),
            BackendError::Unavailable
        ));
    }

    #[test]
    fn bound_unsupported_profile_is_a_definitive_prelaunch_refusal() {
        let error = bound_error(startup_refusal(
            "unsupported_agent_kind",
            "Strict startup does not support this shell or script profile",
        ));
        assert!(
            matches!(error, BackendError::Refused { code: Some(code), .. }
            if code == "unsupported_agent_kind")
        );
        assert!(matches!(
            bound_error(startup_refusal(
                "unsupported_unknown",
                "undocumented outcome"
            )),
            BackendError::Unavailable
        ));
    }

    #[test]
    fn bound_start_receipts_cannot_change_operation_or_pane() {
        let request = StartAgent {
            pane_id: PaneId::new("w1:p1"),
            kind: "codex".into(),
            command: "codex".into(),
            executable: None,
            args: vec![],
            timeout_ms: 5000,
        };
        let response = json!({"result":{"type":"agent_started_bound","operation_id":"op","launch_id":"immutable","agent":{"pane_id":"w1:p1","agent":"codex"},"argv":["codex"]}});
        assert_eq!(
            started_bound_agent(&response, &request, "op")
                .unwrap()
                .launch_id
                .as_deref(),
            Some("immutable")
        );
        for (pointer, value) in [
            ("/result/operation_id", json!("wrong")),
            ("/result/launch_id", json!("")),
            ("/result/agent/pane_id", json!("w2:p2")),
            ("/result/agent/agent", json!("claude")),
            ("/result/argv", json!([42])),
        ] {
            let mut invalid = response.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(started_bound_agent(&invalid, &request, "op").is_err());
        }
    }
    #[test]
    fn collaboration_capability_requires_a_released_supported_version() {
        for version in ["0.9.0", "v0.9.1", "0.10.0", "1.0.0", "0.9.0+build"] {
            assert_eq!(
                super::collaboration_capabilities(Some(version)),
                vec!["agent_collaboration"]
            );
        }
        for version in [
            Some("0.8.9"),
            Some("0.9.0-rc.1"),
            Some("0.9"),
            Some("x"),
            None,
        ] {
            assert!(super::collaboration_capabilities(version).is_empty());
        }
    }
    #[test]
    fn managed_profile_discovery_distinguishes_empty_unknown_and_invalid() {
        let mut response = json!({"result":{"capabilities":{"agent_start_bound":true,"agent_get_bound":true,"bound_agent_kinds":["codex","opencode"]}}});
        assert_eq!(
            metadata_bound_agent_kinds(&response),
            Some(vec!["codex".into(), "opencode".into()])
        );
        response["result"]["capabilities"]["bound_agent_kinds"] = json!([]);
        assert_eq!(metadata_bound_agent_kinds(&response), Some(vec![]));
        for invalid in [
            Value::Null,
            json!("codex"),
            json!(["codex", "codex"]),
            json!(["/bin/sh"]),
            json!([1]),
        ] {
            response["result"]["capabilities"]["bound_agent_kinds"] = invalid;
            assert!(metadata_bound_agent_kinds(&response).is_none());
        }
        response["result"]["capabilities"]["bound_agent_kinds"] = json!(["codex"]);
        response["result"]["capabilities"]["agent_start_bound"] = json!(false);
        assert!(metadata_bound_agent_kinds(&response).is_none());
    }

    #[test]
    fn bound_discovery_identity_survives_status_and_changes_with_runtime() {
        let mut value = json!({"pane_id":"w1:p1", "terminal_id":"term", "launch_id":"launch-one", "owner_epoch":"owner", "name":"bound-name", "agent":"codex", "agent_status":"idle"});
        for status in ["idle", "working", "done", "blocked"] {
            value["agent_status"] = json!(status);
            let agent = agent_from_json(&value).unwrap();
            assert_eq!(agent.launch_id.as_deref(), Some("launch-one"));
            assert_eq!(agent.instance_id.as_deref(), Some("launch-one"));
        }
        value["launch_id"] = json!("launch-two");
        assert_eq!(
            agent_from_json(&value).unwrap().instance_id.as_deref(),
            Some("launch-two")
        );
        value["owner_epoch"] = Value::Null;
        assert!(agent_from_json(&value).is_err());
        assert!(agent_instance_id(&value).is_none());
        value.as_object_mut().unwrap().remove("launch_id");
        assert!(agent_from_json(&value).unwrap().instance_id.is_none());
    }

    #[test]
    fn agent_identity_is_conversation_scoped_not_pane_scoped() {
        let first = serde_json::json!({"terminal_id":"term", "agent_session":{"value":"first"}});
        let replacement =
            serde_json::json!({"terminal_id":"term", "agent_session":{"value":"second"}});
        assert_ne!(
            super::agent_instance_id(&first),
            super::agent_instance_id(&replacement)
        );
        assert!(super::agent_instance_id(&first).is_some());
        assert!(super::agent_instance_id(
            &serde_json::json!({"pane_id":"w1:p1", "terminal_id":"term"})
        )
        .is_none());
        assert!(super::agent_instance_id(
            &serde_json::json!({"terminal_id":"", "agent_session":{"value":"first"}})
        )
        .is_none());
    }

    #[test]
    fn launch_identity_survives_late_session_metadata_but_not_another_launch() {
        let mut agent =
            serde_json::json!({ "terminal_id": "term", "name": "muqun-0123456789abcdef0123" });
        let first = super::agent_instance_id(&agent).unwrap();
        agent["agent_session"] = serde_json::json!({ "value": "reported-later" });
        assert_eq!(
            super::agent_instance_id(&agent).as_deref(),
            Some(first.as_str())
        );
        agent["name"] = serde_json::json!("muqun-0123456789abcdef0124");
        assert_ne!(
            super::agent_instance_id(&agent).as_deref(),
            Some(first.as_str())
        );
        agent["name"] = serde_json::json!("reviewer");
        assert_ne!(
            super::agent_instance_id(&agent).as_deref(),
            Some(first.as_str())
        );
    }
    use std::sync::{Arc, Mutex};

    use super::*;
    use tokio_stream::StreamExt as _;

    #[test]
    fn startup_requires_interactive_readiness_and_the_original_occupant() {
        let ready = json!({ "name": "helper", "terminal_id": "term", "agent": "claude", "agent_status": "idle", "interactive_ready": true });
        assert!(startup_ready(&ready, "helper", "term", "claude").unwrap());
        for status in ["working", "unknown"] {
            let mut waiting = ready.clone();
            waiting["agent_status"] = json!(status);
            assert!(!startup_ready(&waiting, "helper", "term", "claude").unwrap());
        }
        for (key, value) in [
            ("agent_status", "blocked"),
            ("name", "replaced"),
            ("terminal_id", "replaced"),
            ("agent", "codex"),
        ] {
            let mut changed = ready.clone();
            changed[key] = json!(value);
            assert!(startup_ready(&changed, "helper", "term", "claude").is_err());
        }
        let mut launching = ready.clone();
        launching["interactive_ready"] = json!(false);
        launching["launch_pending"] = json!(true);
        assert!(!startup_ready(&launching, "helper", "term", "claude").unwrap());
        launching["launch_pending"] = json!(false);
        assert!(startup_ready(&launching, "helper", "term", "claude").is_err());
    }

    #[test]
    fn startup_retry_requires_an_explicit_pre_input_shell_refusal() {
        assert!(can_retry_agent_start(&startup_refusal(
            "agent_pane_busy",
            "shell starting"
        )));
        for code in [
            "agent_start_input_failed",
            "agent_not_ready",
            "timeout",
            "agent_pane_unavailable",
        ] {
            assert!(!can_retry_agent_start(&startup_refusal(code, "failure")));
        }
        assert!(!can_retry_agent_start(&BackendError::Unavailable));
    }

    struct FakeHerdr {
        socket_path: PathBuf,
        calls: Arc<Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    #[tokio::test]
    async fn two_assistants_of_one_kind_receive_distinct_live_names() {
        let fake = FakeHerdr::start();
        let backend = HerdrBackend::new(fake.socket_path.clone());
        for pane in ["w1:p2", "w1:p3"] {
            backend
                .start_agent(&StartAgent {
                    pane_id: PaneId::new(pane),
                    kind: "claude".into(),
                    command: "claude".into(),
                    executable: None,
                    args: vec![],
                    timeout_ms: 30_000,
                })
                .await
                .unwrap();
        }
        let calls = fake.calls.lock().unwrap();
        let names: Vec<_> = calls
            .iter()
            .filter(|call| call["method"] == "agent.start")
            .map(|call| {
                assert_eq!(call["params"]["kind"], "claude");
                call["params"]["name"].as_str().unwrap()
            })
            .collect();
        assert_eq!(names.len(), 2);
        assert_ne!(names[0], names[1]);
        assert!(names
            .iter()
            .all(|name| name.starts_with("muqun-") && name.len() <= 32));
    }

    impl FakeHerdr {
        fn start_responses(responses: Vec<Value>) -> Self {
            let socket_path = crate::short_test_socket("gw-bound-herdr");
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let task = tokio::spawn(async move {
                let mut responses = std::collections::VecDeque::from(responses);
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    recorded.lock().unwrap().push(request.clone());
                    let mut response = responses.pop_front().expect("unexpected native RPC");
                    response["id"] = request["id"].clone();
                    let mut stream = reader.into_inner();
                    stream
                        .write_all(response.to_string().as_bytes())
                        .await
                        .unwrap();
                    stream.write_all(b"\n").await.unwrap();
                }
            });
            Self {
                socket_path,
                calls,
                task,
            }
        }
        fn start() -> Self {
            Self::start_refusing(&[])
        }

        /// A herdr that does not know `refused` -- the shape of one older
        /// than a method this gateway asks for optionally.
        fn start_refusing(refused: &'static [&'static str]) -> Self {
            Self::start_scripted(refused, None)
        }

        fn start_scripted(
            refused: &'static [&'static str],
            startup: Option<Vec<&'static str>>,
        ) -> Self {
            let socket_path = crate::short_test_socket("gw-herdr");
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let task = tokio::spawn(async move {
                let mut startup = startup.map(std::collections::VecDeque::from);
                let mut started = Value::Null;
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap_or_default();
                    recorded.lock().unwrap().push(request.clone());
                    let response = if method == "agent.start"
                        && startup
                            .as_ref()
                            .is_some_and(|states| states.front() == Some(&"shell-busy"))
                    {
                        startup.as_mut().unwrap().pop_front();
                        json!({ "id": request["id"], "error": { "code": "agent_pane_busy", "message": "shell is starting" } })
                    } else if refused.contains(&method) {
                        json!({
                            "id": request["id"],
                            "error": { "code": "method_not_found", "message": format!("unknown method {method}") }
                        })
                    } else {
                        let result = if startup.is_some() && method == "agent.start" {
                            started = json!({
                                "name": request["params"]["name"],
                                "pane_id": request["params"]["pane_id"],
                                "agent": request["params"]["kind"],
                                "terminal_id": "startup-terminal",
                                "agent_status": "unknown", "launch_pending": true,
                            });
                            json!({ "agent": started })
                        } else if let Some(states) =
                            startup.as_mut().filter(|_| method == "agent.get")
                        {
                            assert_eq!(request["params"]["target"], started["name"]);
                            let status = states.pop_front().unwrap_or("unknown");
                            started["agent_status"] = json!(if status == "idle-without-identity" {
                                "idle"
                            } else {
                                status
                            });
                            started["interactive_ready"] =
                                json!(status == "idle" || status == "idle-without-identity");
                            if status == "idle" {
                                started["agent_session"] =
                                    json!({ "value": "startup-conversation" });
                            }
                            json!({ "agent": started })
                        } else {
                            fake_result(method)
                        };
                        json!({ "id": request["id"], "result": result })
                    };
                    let mut stream = reader.into_inner();
                    stream
                        .write_all(response.to_string().as_bytes())
                        .await
                        .unwrap();
                    stream.write_all(b"\n").await.unwrap();
                }
            });
            Self {
                socket_path,
                calls,
                task,
            }
        }
    }

    impl Drop for FakeHerdr {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    #[tokio::test]
    async fn old_native_metadata_never_receives_a_bound_mutation() {
        let fake = FakeHerdr::start_responses(vec![json!({"result":{"version":"99.0.0"}})]);
        let backend = HerdrBackend::new(&fake.socket_path);
        let request = BoundPrompt {
            operation_id: "op".into(),
            expected_launch_id: "launch".into(),
            text: "hello".into(),
        };
        assert!(matches!(
            backend.prompt_bound_agent(&request).await,
            Err(BackendError::Unsupported("instance_bound_prompt"))
        ));
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["ping"]
        );
    }

    fn interrupt_request() -> BoundInterrupt {
        BoundInterrupt {
            operation_id: "interrupt-op".into(),
            expected_launch_id: "launch".into(),
            expected_owner_epoch: "owner".into(),
        }
    }

    fn interrupt_metadata() -> Value {
        json!({"result":{"capabilities":{"agent_interrupt_bound":true,"agent_lifecycle_bound":true,"owner_epoch":"owner"}}})
    }

    fn interrupt_ack() -> Value {
        json!({"result":{"type":"agent_interrupted_bound","operation_id":"interrupt-op","launch_id":"launch","owner_epoch":"owner","receipt_id":"receipt","key":"Escape","bytes_written":1,"input_disposition":"written"}})
    }

    #[tokio::test]
    async fn bound_interrupt_invalid_identity_never_contacts_native() {
        let absent =
            std::env::temp_dir().join(format!("absent-interrupt-{}.sock", uuid::Uuid::new_v4()));
        let backend = HerdrBackend::new(&absent);
        for invalid in [String::new(), "x".repeat(129), "bad\nidentity".into()] {
            let mut request = interrupt_request();
            request.expected_launch_id = invalid;
            assert!(
                matches!(backend.interrupt_bound_agent(&request).await, Err(BackendError::Refused {code:Some(code),..}) if code=="invalid_request")
            );
        }
        assert!(!absent.exists());
    }

    #[tokio::test]
    async fn bound_interrupt_uses_only_exact_native_control_without_readiness_or_keys() {
        let fake = FakeHerdr::start_responses(vec![interrupt_metadata(), interrupt_ack()]);
        let result = HerdrBackend::new(&fake.socket_path)
            .interrupt_bound_agent(&interrupt_request())
            .await
            .unwrap();
        assert_eq!(result.receipt_id, "receipt");
        assert_eq!(result.bytes_written, 1);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["ping", "agent.interrupt_bound"]
        );
        assert_eq!(
            calls[1]["params"],
            json!({"operation_id":"interrupt-op","expected_launch_id":"launch","expected_owner_epoch":"owner"})
        );
    }

    #[tokio::test]
    async fn bound_interrupt_malformed_receipt_is_uncertain_and_never_retried() {
        for (field, value) in [
            ("type", json!("agent_prompted_bound")),
            ("operation_id", json!("other")),
            ("launch_id", json!("other")),
            ("owner_epoch", json!("other")),
            ("receipt_id", json!("")),
            ("receipt_id", json!("bad\nreceipt")),
            ("key", json!("Enter")),
            ("bytes_written", json!(2)),
            ("bytes_written", json!(0)),
            ("bytes_written", json!("1")),
            ("input_disposition", json!("queued")),
        ] {
            let mut ack = interrupt_ack();
            ack["result"][field] = value;
            let fake = FakeHerdr::start_responses(vec![interrupt_metadata(), ack]);
            let result = HerdrBackend::new(&fake.socket_path)
                .interrupt_bound_agent(&interrupt_request())
                .await;
            assert!(
                matches!(result, Err(BackendError::InvalidResponse(_))),
                "{field}: {result:?}"
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn bound_interrupt_requires_boolean_capability_and_owner_evidence() {
        for metadata in [
            json!({"result":{"version":"999.0.0"}}),
            json!({"result":{"capabilities":{"agent_interrupt_bound":"true","agent_lifecycle_bound":true,"owner_epoch":"owner"}}}),
            json!({"result":{"capabilities":{"agent_interrupt_bound":true,"agent_lifecycle_bound":false,"owner_epoch":"owner"}}}),
            json!({"result":{"capabilities":{"agent_interrupt_bound":true,"agent_lifecycle_bound":true,"owner_epoch":""}}}),
        ] {
            let fake = FakeHerdr::start_responses(vec![metadata]);
            assert!(matches!(
                HerdrBackend::new(&fake.socket_path)
                    .interrupt_bound_agent(&interrupt_request())
                    .await,
                Err(BackendError::Unsupported("instance_bound_interrupt"))
            ));
            assert_eq!(fake.calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn bound_interrupt_capacity_refusal_is_distinct_from_unknown_effects() {
        for code in [
            "resource_limit",
            "instance_changed",
            "unsupported",
            "request_key_conflict",
            "delivery_unconfirmed",
            "unknown_error",
        ] {
            let fake = FakeHerdr::start_responses(vec![
                interrupt_metadata(),
                json!({"error":{"code":code,"message":"bounded failure"}}),
            ]);
            let result = HerdrBackend::new(&fake.socket_path)
                .interrupt_bound_agent(&interrupt_request())
                .await;
            if matches!(code, "delivery_unconfirmed" | "unknown_error") {
                assert!(matches!(result, Err(BackendError::Unavailable)));
            } else {
                assert!(
                    matches!(result, Err(BackendError::Refused {code:Some(ref actual),..}) if actual==code)
                );
            }
            assert_eq!(fake.calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn native_bound_start_and_prompt_use_new_methods_and_validate_receipts() {
        let metadata = json!({"result":{"capabilities":{"agent_start_bound":true,"agent_prompt_bound":true,"agent_get_bound":true}}});
        let fake = FakeHerdr::start_responses(vec![
            metadata.clone(),
            json!({"result":{
                "type":"agent_started_bound","operation_id":"start-op","launch_id":"new-launch",
                "agent":{"pane_id":"w1:p1","agent":"codex","agent_status":"idle"},"argv":["/bin/codex"]
            }}),
            metadata,
            json!({"result":{"type":"agent_bound_info","launch_id":"new-launch","visible_text":"> Ask Codex anything","agent":{"agent_status":"idle","interactive_ready":true}}}),
            json!({"result":{"type":"agent_prompted_bound","operation_id":"prompt-op","launch_id":"new-launch","bytes_written":5,"input_disposition":"written"}}),
        ]);
        let backend = HerdrBackend::new(&fake.socket_path);
        let request = StartAgent {
            pane_id: PaneId::new("w1:p1"),
            kind: "codex".into(),
            command: "codex".into(),
            executable: Some(PathBuf::from("/bin/codex")),
            args: vec![],
            timeout_ms: 5000,
        };
        let started = backend
            .start_bound_agent(
                &request,
                "start-op",
                Some("/private/work/context.json"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(started.launch_id.as_deref(), Some("new-launch"));
        assert!(started.instance_id.is_none());
        let receipt = backend
            .prompt_bound_agent(&BoundPrompt {
                expected_launch_id: started.launch_id.unwrap(),
                operation_id: "prompt-op".into(),
                text: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(receipt.operation_id, "prompt-op");
        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "ping",
                "agent.start_bound",
                "ping",
                "agent.get_bound",
                "agent.prompt_bound"
            ]
        );
        assert_eq!(calls[1]["params"]["command"], "/bin/codex");
        assert_eq!(
            calls[1]["params"]["work_context_file"],
            "/private/work/context.json"
        );
        assert!(calls[4]["params"].get("target").is_none());
    }
    #[tokio::test]
    async fn reporting_start_requires_native_capability_and_configuration_receipt() {
        for (capability, receipt) in [(false, false), (true, false), (true, true)] {
            let metadata = json!({"result":{"capabilities":{"agent_start_bound":true,"agent_get_bound":true,"agent_lifecycle_bound":true,"agent_reporting_mcp_codex":capability,"owner_epoch":"epoch"}}});
            let mut response = json!({"result":{"type":"agent_started_bound","operation_id":"start-op","launch_id":"launch","owner_epoch":"epoch","agent_kind":"codex","argv":["codex"],"agent":{"pane_id":"w1:p1","terminal_id":"term","agent":"codex"}}});
            if receipt {
                response["result"]["reporting_mcp"] = json!("codex_stdio_v1");
            }
            let fake = FakeHerdr::start_responses(vec![metadata, response]);
            let backend = HerdrBackend::new(&fake.socket_path);
            let request = StartAgent {
                pane_id: PaneId::new("w1:p1"),
                kind: "codex".into(),
                command: "codex".into(),
                executable: None,
                args: vec![],
                timeout_ms: 5000,
            };
            let reporting = super::super::ReportingMcp {
                executable: "/private/reporter".into(),
                sha256: "a".repeat(64),
            };
            let result = backend
                .start_bound_agent(
                    &request,
                    "start-op",
                    Some("/private/context.json"),
                    Some(&reporting),
                )
                .await;
            if !capability {
                assert!(matches!(
                    result,
                    Err(BackendError::Unsupported("reporting_mcp_codex"))
                ));
            } else if !receipt {
                assert!(matches!(result, Err(BackendError::InvalidResponse(_))));
            } else {
                assert_eq!(result.unwrap().launch_id.as_deref(), Some("launch"));
            }
            let calls = fake.calls.lock().unwrap();
            assert_eq!(calls.len(), if capability { 2 } else { 1 });
            if capability {
                assert_eq!(calls[1]["method"], "agent.start_bound");
                assert_eq!(
                    calls[1]["params"]["reporting_mcp"]["executable"],
                    reporting.executable
                );
                assert!(calls[1]["params"].get("config").is_none());
            }
        }
    }

    #[tokio::test]
    async fn native_no_process_proof_requires_matching_operation_and_discovered_epoch() {
        for (matching, code) in [
            (true, "unsupported_agent_kind"),
            (true, "agent_not_ready"),
            (false, "unsupported_agent_kind"),
        ] {
            let metadata = json!({"result":{"capabilities":{"agent_start_bound":true,"agent_get_bound":true,"agent_lifecycle_bound":true,"owner_epoch":"epoch"}}});
            let response = json!({"error":{"code":code,"message":"No child"},"start_receipt":{"effect":"not_started","operation_id":if matching {"start-op"}else{"other-op"},"owner_epoch":"epoch","receipt_id":"refusal-proof"}});
            let fake = FakeHerdr::start_responses(vec![metadata, response]);
            let backend = HerdrBackend::new(&fake.socket_path);
            let request = StartAgent {
                pane_id: PaneId::new("w1:p1"),
                kind: "codex".into(),
                command: "codex".into(),
                executable: None,
                args: vec![],
                timeout_ms: 5000,
            };
            let result = backend
                .start_bound_agent(&request, "start-op", None, None)
                .await;
            assert_eq!(
                matches!(&result, Err(BackendError::StartNotStarted { .. })),
                matching
            );
            if matching {
                assert!(
                    matches!(result, Err(BackendError::StartNotStarted {code: Some(actual), ..}) if actual == code)
                );
            }
            let calls = fake.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1]["method"], "agent.start_bound");
        }
    }
    #[tokio::test]
    async fn prompt_preflight_checks_exact_launch_and_approval_without_writing_input() {
        for (launch, status, ready) in [
            ("original", "idle", true),
            ("replaced", "idle", true),
            ("original", "blocked", false),
        ] {
            let metadata = json!({"result":{"capabilities":{"agent_get_bound":true,"agent_prompt_bound":true}}});
            let observation = json!({"result":{"type":"agent_bound_info","launch_id":launch,"visible_text":"","agent":{"agent_status":status,"interactive_ready":ready,"launch_pending":false}}});
            let fake = FakeHerdr::start_responses(vec![metadata, observation]);
            let backend = HerdrBackend::new(&fake.socket_path);
            assert_eq!(
                backend.preflight_bound_prompt("original").await.is_ok(),
                launch == "original" && ready
            );
            let calls = fake.calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[1]["method"], "agent.get_bound");
            assert_eq!(calls[1]["params"]["expected_launch_id"], "original");
        }
    }

    #[tokio::test]
    async fn bound_native_idle_with_codex_trust_never_receives_a_prompt() {
        let screen = include_str!("../../tests/fixtures/approval-codex-0154-trust.txt");
        let fake = FakeHerdr::start_responses(vec![
            json!({"result":{"capabilities":{"agent_prompt_bound":true,"agent_get_bound":true}}}),
            json!({"result":{"type":"agent_bound_info","launch_id":"launch","visible_text":screen,"agent":{"agent_status":"idle","interactive_ready":true}}}),
        ]);
        let backend = HerdrBackend::new(&fake.socket_path);
        let result = backend
            .prompt_bound_agent(&BoundPrompt {
                expected_launch_id: "launch".into(),
                operation_id: "op".into(),
                text: "never approve".into(),
            })
            .await;
        assert!(
            matches!(result,Err(BackendError::Refused {code:Some(code),..}) if code=="agent_blocked")
        );
        assert_eq!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["ping", "agent.get_bound"]
        );
    }

    #[tokio::test]
    async fn bound_readiness_timeout_and_replacement_never_submit() {
        for (launch, status, expected) in [
            ("launch", "unknown", "agent_not_ready"),
            ("replacement", "idle", "instance_changed"),
        ] {
            let fake = FakeHerdr::start_responses(vec![
                json!({"result":{"capabilities":{"agent_prompt_bound":true,"agent_get_bound":true}}}),
                json!({"result":{"type":"agent_bound_info","launch_id":launch,"visible_text":"starting","agent":{"agent_status":status,"interactive_ready":false}}}),
            ]);
            let backend =
                HerdrBackend::with_request_timeout(&fake.socket_path, Duration::from_millis(50));
            let result = backend
                .prompt_bound_agent(&BoundPrompt {
                    expected_launch_id: "launch".into(),
                    operation_id: "op".into(),
                    text: "hello".into(),
                })
                .await;
            assert!(
                matches!(result,Err(BackendError::Refused {code:Some(code),..}) if code==expected)
            );
            assert_eq!(fake.calls.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn bound_prompt_ambiguous_acknowledgement_is_never_retried() {
        for response in [
            json!({"result":{"type":"agent_prompted_bound","operation_id":"wrong-op","launch_id":"launch","bytes_written":5,"input_disposition":"written"}}),
            json!({"error":{"code":"delivery_unconfirmed","message":"partial submission"}}),
        ] {
            let fake = FakeHerdr::start_responses(vec![
                json!({"result":{"capabilities":{"agent_prompt_bound":true,"agent_get_bound":true}}}),
                json!({"result":{"type":"agent_bound_info","launch_id":"launch","visible_text":"> Ask anything","agent":{"agent_status":"idle","interactive_ready":true}}}),
                response,
            ]);
            let result = HerdrBackend::new(&fake.socket_path)
                .prompt_bound_agent(&BoundPrompt {
                    expected_launch_id: "launch".into(),
                    operation_id: "op".into(),
                    text: "hello".into(),
                })
                .await;
            assert!(matches!(
                result,
                Err(BackendError::InvalidResponse(_) | BackendError::Unavailable)
            ));
            assert_eq!(
                fake.calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|call| call["method"] == "agent.prompt_bound")
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn raw_start_acknowledgement_is_not_interactive_readiness() {
        for (states, timeout_ms, expected_error) in [
            (vec!["unknown", "idle"], 1000, None),
            (vec!["shell-busy", "idle"], 1000, None),
            (vec!["idle-without-identity", "idle"], 1000, None),
            (vec!["idle-without-identity"], 1000, None),
            (vec!["blocked"], 1000, Some("agent_not_ready")),
            (vec!["unknown"], 20, Some("agent_start_timeout")),
        ] {
            let expected_starts = if states.first() == Some(&"shell-busy") {
                2
            } else {
                1
            };
            let fake = FakeHerdr::start_scripted(&[], Some(states));
            let backend = HerdrBackend::new(fake.socket_path.clone());
            let result = backend
                .start_agent(&StartAgent {
                    pane_id: PaneId::new("w1:p2"),
                    kind: "claude".into(),
                    command: "claude".into(),
                    executable: None,
                    args: vec![],
                    timeout_ms,
                })
                .await;
            match expected_error {
                None => {
                    let started = result.unwrap();
                    let identity: Value =
                        serde_json::from_str(&started.instance_id.unwrap()).unwrap();
                    assert_eq!(identity[0], "startup-terminal");
                    assert_eq!(identity[1], "launch");
                    assert_eq!(identity[2].as_str(), started.target.as_deref());
                }
                Some(expected) => assert!(
                    matches!(result, Err(BackendError::Refused { code: Some(code), .. }) if code == expected)
                ),
            }
            let calls = fake.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["method"] == "agent.start")
                    .count(),
                expected_starts
            );
            assert!(calls.iter().any(|call| call["method"] == "agent.get"));
            assert!(
                !calls
                    .iter()
                    .any(|call| call["method"] == "agent.prompt"
                        || call["method"] == "pane.send_keys")
            );
        }
    }

    fn fake_result(method: &str) -> Value {
        let workspace = json!({
            "workspace_id": "w1", "label": "work", "focused": true,
            "active_tab_id": "t1", "tab_count": 1,
            "worktree": { "repo_root": "/work", "checkout_path": "/work/task" }
        });
        let tab = json!({
            "tab_id": "t1", "workspace_id": "w1", "label": "shell",
            "focused": true, "active_pane_id": "p1", "pane_count": 1
        });
        let pane = json!({
            "pane_id": "p1", "terminal_id": "terminal-1", "workspace_id": "w1",
            "tab_id": "t1", "foreground_cwd": "/work/task", "focused": true,
            "width": 120, "height": 40, "agent": "claude", "agent_status": "idle",
            "scroll": { "max_offset_from_bottom": 90, "viewport_rows": 40 }
        });
        let agent = json!({
            "target": "p1", "pane_id": "p1", "workspace_id": "w1", "tab_id": "t1",
            "agent": "claude", "display_agent": "Claude", "agent_status": "idle",
            "state_change_seq": 7
        });
        match method {
            "ping" => json!({ "version": "contract", "protocol": 17 }),
            "workspace.list" => json!({ "workspaces": [workspace] }),
            "tab.list" => json!({ "tabs": [tab] }),
            "pane.list" => json!({ "panes": [pane] }),
            "agent.list" => json!({ "agents": [agent] }),
            "pane.get" => json!({ "pane": pane }),
            "agent.get" => json!({ "agent": agent }),
            "pane.read" => json!({ "read": { "text": "contract output", "revision": 9 } }),
            // As herdr 0.8.2 answered for a real nvim pane, minus the shell
            // pid: a test has no pty of that pid's to be measured.
            "pane.process_info" => json!({
                "process_info": {
                    "pane_id": "p1",
                    "foreground_process_group_id": 36692,
                    "foreground_processes": [{
                        "pid": 36692, "name": "nvim", "argv0": "nvim",
                        "argv": ["nvim", "notes.md"], "cmdline": "nvim notes.md",
                        "cwd": "/work/task"
                    }]
                }
            }),
            "workspace.create" => json!({ "workspace": workspace }),
            "tab.create" => json!({ "tab": tab }),
            "pane.split" => json!({ "pane": pane }),
            "agent.start" => json!({ "argv": ["claude", "--resume"] }),
            "worktree.list" => json!({
                "worktrees": [{ "path": "/work/task", "branch": "refs/heads/task" }]
            }),
            "worktree.open" | "worktree.create" => json!({
                "workspace": workspace,
                "root_pane": pane,
                "worktree": { "path": "/work/task" }
            }),
            _ => json!({ "ok": true }),
        }
    }

    /// herdr's pane payload names no foreground program, so the one thing
    /// the app's editor detection keys on was `None` for every herdr pane
    /// and an nvim pane was read, folded and drawn as a shell (the joined
    /// 178-column rows in the card). `pane.process_info` knows, and a pane
    /// listing now carries its answer -- once per pane, per listing.
    #[tokio::test]
    async fn a_herdr_pane_names_its_foreground_program_from_process_info() {
        let herdr = FakeHerdr::start();
        let backend = HerdrBackend::new(&herdr.socket_path);

        let panes = backend.list_panes().await.unwrap();
        assert_eq!(panes[0].foreground_command.as_deref(), Some("nvim"));
        let pane = backend.get_pane(&PaneId::new("p1")).await.unwrap();
        assert_eq!(pane.foreground_command.as_deref(), Some("nvim"));
        // And it reaches the wire the app reads, where `ScrollbackStore`
        // and the app's `isFullScreenTuiPane` both look for it.
        let envelope = super::super::compat::pane_list(panes);
        assert_eq!(envelope["result"]["panes"][0]["foreground_command"], "nvim");

        let calls = herdr.calls.lock().unwrap().clone();
        let asked: Vec<&str> = calls
            .iter()
            .filter(|call| call["method"] == "pane.process_info")
            .map(|call| call["params"]["pane_id"].as_str().unwrap())
            .collect();
        assert_eq!(asked, ["p1", "p1"], "one process_info per pane per call");
    }

    /// The protocol floor admits a herdr older than `pane.process_info`.
    /// Its panes must come back exactly as they did before the call existed,
    /// and it must not be asked again on every listing.
    #[tokio::test]
    async fn a_herdr_without_process_info_is_asked_once_and_left_alone() {
        let herdr = FakeHerdr::start_refusing(&["pane.process_info"]);
        let backend = HerdrBackend::new(&herdr.socket_path);

        for _ in 0..3 {
            let panes = backend.list_panes().await.unwrap();
            assert_eq!(panes[0].foreground_command, None);
            assert_eq!(panes[0].width, Some(120), "the parsed pane is untouched");
        }
        let calls = herdr.calls.lock().unwrap().clone();
        let asked = calls
            .iter()
            .filter(|call| call["method"] == "pane.process_info")
            .count();
        assert_eq!(asked, 1);
    }

    #[test]
    fn the_foreground_command_is_the_group_leader_named_as_tmux_would() {
        // `cat | less`: the leader owns the terminal and is what tmux reports.
        let info = json!({
            "foreground_process_group_id": 20,
            "foreground_processes": [
                { "pid": 21, "name": "cat", "argv0": "cat" },
                { "pid": 20, "name": "less", "argv0": "/usr/bin/less" }
            ]
        });
        assert_eq!(
            foreground_command_from_process_info(&info).as_deref(),
            Some("less")
        );
        // Claude Code, as herdr 0.8.2 reports it: the node binary's own name
        // under `name`, the program under `argv0`.
        let info = json!({
            "foreground_process_group_id": 36694,
            "foreground_processes": [{ "pid": 36694, "name": "2.1.263", "argv0": "claude" }]
        });
        assert_eq!(
            foreground_command_from_process_info(&info).as_deref(),
            Some("claude")
        );
        // A login shell's dash is not part of its name.
        let info = json!({
            "foreground_process_group_id": 22270,
            "foreground_processes": [{ "pid": 22270, "name": "zsh", "argv0": "zsh", "cmdline": "-zsh" }]
        });
        assert_eq!(
            foreground_command_from_process_info(&info).as_deref(),
            Some("zsh")
        );
        let info =
            json!({ "foreground_processes": [{ "pid": 1, "name": "bash", "argv0": "-bash" }] });
        assert_eq!(
            foreground_command_from_process_info(&info).as_deref(),
            Some("bash")
        );
        // No leader among them: the first listed stands in. No `argv0`: the
        // name does. Nothing listed: nothing claimed.
        let info = json!({
            "foreground_process_group_id": 99,
            "foreground_processes": [{ "pid": 5, "name": "vim" }, { "pid": 6, "name": "sh" }]
        });
        assert_eq!(
            foreground_command_from_process_info(&info).as_deref(),
            Some("vim")
        );
        assert_eq!(
            foreground_command_from_process_info(&json!({ "foreground_processes": [] })),
            None
        );
        assert_eq!(foreground_command_from_process_info(&json!({})), None);
    }

    /// The width comes off the pty the shell runs on, which is the one herdr
    /// sized: a pty made here and set to 59 rows by 31 columns -- the grid
    /// of the narrow pane in the card -- answers exactly that, both asked
    /// by path and asked by the pid of a process it is the terminal of.
    #[cfg(unix)]
    #[test]
    fn a_pane_is_as_wide_as_the_terminal_its_shell_runs_on() {
        use std::os::fd::FromRawFd as _;
        use std::os::unix::process::CommandExt as _;

        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let size = libc::winsize {
            ws_row: 59,
            ws_col: 31,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `openpty` writes two descriptors and reads the winsize it
        // is handed; every pointer is to a live local.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &size,
            )
        };
        assert_eq!(opened, 0, "openpty failed");
        // SAFETY: `master` is the descriptor `openpty` just returned, and
        // `ptsname` answers with a static buffer copied out before any other
        // pty call.
        let slave_path = unsafe {
            PathBuf::from(
                std::ffi::CStr::from_ptr(libc::ptsname(master))
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        assert_eq!(tty::window_size(&slave_path), Some((31, 59)));

        // A child whose controlling terminal is that pty, the way a shell in
        // a pane has herdr's pty: a session of its own, with the slave as its
        // terminal.
        // SAFETY: `slave` is the descriptor `openpty` returned and this is
        // its only owner from here on.
        let stdin = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut command = std::process::Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(std::process::Stdio::from(stdin))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: `setsid` and one ioctl on fd 0, both async-signal-safe,
        // and nothing allocated between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn a child on the pty");

        let backend = HerdrBackend::new("/nonexistent/herdr.sock");
        let grid = backend.pane_grid(child.id());
        // Asked twice: the second answer comes through the cache and must be
        // the same one.
        let again = backend.pane_grid(child.id());
        let _ = child.kill();
        let _ = child.wait();
        // SAFETY: `master` is still open and owned here.
        unsafe { libc::close(master) };
        assert_eq!(grid, Some((31, 59)));
        assert_eq!(again, Some((31, 59)));

        // A pid without a terminal -- or one this process may not ask about
        // -- is not a width.
        assert_eq!(backend.pane_grid(u32::MAX - 1), None);
    }

    #[test]
    fn herdr_never_constructs_a_range() {
        // herdr's `pane.read` has no range parameter, so it can never honour
        // one it was asked for -- and any range it invented from a tail it
        // merely happened to serve would be indistinguishable, on the wire,
        // from a genuinely honoured request. `None` unconditionally is the
        // only answer that does not lie about that, on every pane shape:
        // a short read, a long one, and the empty read a fresh pane starts
        // from.
        assert_eq!(herdr_pane_output("line1\nline2\nline3").range, None);
        assert_eq!(herdr_pane_output("line1\nline2").range, None);
        assert_eq!(herdr_pane_output("").range, None);
    }

    #[test]
    fn pane_output_from_response_never_reports_a_range() {
        // Even where herdr's response nests a `scroll.max_offset_from_bottom`
        // count next to the read -- the field this used to read as a
        // fabricated tail-of-total -- it must not turn into a `range`. That
        // count is Herdr's own, unrelated to what a range would mean here,
        // and this backend has no way to honour a requested range regardless
        // of what herdr reports.
        let response = json!({
            "result": {
                "read": {
                    "text": "line1\nline2\nline3",
                    "revision": 4,
                    "scroll": { "max_offset_from_bottom": 900 }
                }
            }
        });
        let output = pane_output_from_response(&response).unwrap();
        assert_eq!(output.revision, Some(4));
        assert_eq!(output.range, None);
    }

    #[test]
    fn pane_parser_preserves_the_released_mobile_contract_fields() {
        let pane = pane_from_json(&json!({
            "pane_id": "wM:p1",
            "terminal_id": "t-9",
            "workspace_id": "wM",
            "tab_id": "wM:t1",
            "terminal_title_stripped": "Claude",
            "foreground_cwd": "/work/muqun",
            "height": 65,
            "scroll": {
                "max_offset_from_bottom": 908,
                "viewport_rows": 64
            }
        }))
        .unwrap();

        assert_eq!(pane.id.as_str(), "wM:p1");
        assert_eq!(pane.terminal_id.as_deref(), Some("t-9"));
        assert_eq!(
            pane.cwd.as_deref(),
            Some(std::path::Path::new("/work/muqun"))
        );
        assert_eq!(pane.max_offset_from_bottom, Some(908));
        assert_eq!(pane.viewport_rows, Some(64));

        let envelope = super::super::compat::pane_list(vec![pane]);
        let pane = &envelope["result"]["panes"][0];
        assert_eq!(pane["terminal_id"], "t-9");
        assert_eq!(pane["terminal_title_stripped"], "Claude");
        assert_eq!(pane["foreground_cwd"], "/work/muqun");
        assert_eq!(pane["scroll"]["max_offset_from_bottom"], 908);
        assert_eq!(pane["scroll"]["viewport_rows"], 64);
    }

    /// A herdr pane payload as herdr actually sends one -- no `width`, no
    /// `height`, no alternate-screen flag -- must still come back with a
    /// height, because `scroll.viewport_rows` is that height.
    ///
    /// Measured against a live herdr at protocol 20: a pane whose shell
    /// reported `stty size` = `37 44` came back with `viewport_rows: 37`.
    #[test]
    fn a_herdr_pane_gets_its_height_from_the_viewport_rows_herdr_does_send() {
        let pane = pane_from_json(&json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "focused": true,
            "scroll": { "offset_from_bottom": 0, "max_offset_from_bottom": 0, "viewport_rows": 37 }
        }))
        .unwrap();
        assert_eq!(pane.height, Some(37));
        assert_eq!(pane.viewport_rows, Some(37));

        // The three herdr genuinely does not have. Each is checked here so
        // that a herdr which starts sending one fails this test rather than
        // silently keeping the workaround alive on the app side.
        assert_eq!(pane.width, None, "herdr has no pane width to give");
        assert_eq!(pane.alternate_on, None, "herdr has no alt-screen flag");
        assert_eq!(pane.cursor_x, None, "herdr exposes no cursor");
        assert_eq!(pane.cursor_y, None);

        // And on the wire.
        let envelope = super::super::compat::pane_list(vec![pane]);
        let pane = &envelope["result"]["panes"][0];
        assert_eq!(pane["height"], 37);
        assert!(pane["width"].is_null());
        assert!(pane["cursor_x"].is_null());
    }

    /// A `height` herdr sends itself wins over the derived one, so this stays
    /// correct if herdr ever starts reporting the field.
    #[test]
    fn a_height_herdr_sends_itself_is_preferred_to_the_viewport_rows() {
        let pane = pane_from_json(&json!({
            "pane_id": "w1:p1",
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "height": 41,
            "scroll": { "max_offset_from_bottom": 0, "viewport_rows": 37 }
        }))
        .unwrap();
        assert_eq!(pane.height, Some(41));
    }

    /// The app now sends every typed character as its own `send-keys`, so the
    /// single character has to survive this adapter untouched.
    ///
    /// Verified end to end against a live herdr and a real nvim: `i`, then
    /// `h` `e` `l` `l` `o` one call each, then `Escape`, `:` `w` `q`, `Enter`
    /// -- nvim obeyed every one and wrote a file containing exactly `hello`.
    /// So herdr's `pane.send_keys` types rather than pastes, and needs no
    /// translation for these characters (literal spaces need a named key).
    #[tokio::test]
    async fn single_characters_reach_herdr_send_keys_unchanged() {
        let herdr = FakeHerdr::start();
        let backend = HerdrBackend::new(&herdr.socket_path);
        for key in ["i", "h", ":", ";", "Escape", "Enter"] {
            backend
                .send_keys(&PaneId::new("w1:p1"), &[key.to_owned()])
                .await
                .unwrap();
        }
        let calls = herdr.calls.lock().unwrap().clone();
        let sent: Vec<String> = calls
            .iter()
            .filter(|call| call["method"] == "pane.send_keys")
            .map(|call| call["params"]["keys"][0].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(sent, ["i", "h", ":", ";", "Escape", "Enter"]);
    }

    #[tokio::test]
    async fn literal_spaces_use_named_keys_without_splitting_the_request() {
        let herdr = FakeHerdr::start();
        let backend = HerdrBackend::new(&herdr.socket_path);
        let keys = [" ", "a", " ", " ", "b", " ", "enter"].map(str::to_owned);
        backend
            .send_keys(&PaneId::new("w1:p1"), &keys)
            .await
            .unwrap();
        let calls = herdr.calls.lock().unwrap();
        let sent: Vec<_> = calls
            .iter()
            .filter(|call| call["method"] == "pane.send_keys")
            .collect();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0]["params"]["keys"],
            json!(["space", "a", "space", "space", "b", "space", "enter"])
        );
    }

    #[test]
    fn workspace_compatibility_keeps_counts_status_and_worktree_paths() {
        let workspace = workspace_from_json(&json!({
            "workspace_id": "wM",
            "number": 3,
            "label": "muqun",
            "focused": true,
            "active_tab_id": "t1",
            "tab_count": 2,
            "pane_count": 5,
            "agent_status": "working",
            "worktree": {
                "repo_root": "/work/muqun",
                "checkout_path": "/work/muqun-task"
            }
        }))
        .unwrap();
        let envelope = super::super::compat::workspace_list(vec![workspace]);
        let workspace = &envelope["result"]["workspaces"][0];

        assert_eq!(workspace["number"], 3);
        assert_eq!(workspace["pane_count"], 5);
        assert_eq!(workspace["agent_status"], "working");
        assert_eq!(workspace["worktree"]["repo_root"], "/work/muqun");
        assert_eq!(workspace["worktree"]["checkout_path"], "/work/muqun-task");
    }

    #[test]
    fn worktree_placement_uses_the_protocol_17_workspace_and_root_pane() {
        let placement = worktree_placement_from_json(&json!({
            "result": {
                "workspace": { "workspace_id": "ws-9" },
                "root_pane": { "pane_id": "pane-9" },
                "worktree": { "path": "/work/task-9" }
            }
        }))
        .unwrap();

        assert_eq!(placement.workspace_id.as_str(), "ws-9");
        assert_eq!(placement.pane_id.as_str(), "pane-9");
        assert_eq!(placement.path, Some(PathBuf::from("/work/task-9")));
    }

    #[test]
    fn an_old_herdr_worktree_method_selects_the_git_fallback() {
        let error = BackendError::Refused {
            code: Some("invalid_request".into()),
            message: "unknown variant `worktree.create`".into(),
        };
        assert!(matches!(
            worktree_error(error),
            BackendError::Unsupported("worktrees")
        ));
    }

    #[test]
    fn agent_status_activity_is_scoped_to_each_known_pane() {
        let panes = ["w1:p1", "w2:p3"]
            .into_iter()
            .map(|pane_id| {
                pane_from_json(&json!({
                    "pane_id": pane_id,
                    "workspace_id": "w1",
                    "tab_id": "t1"
                }))
                .unwrap()
            })
            .collect::<Vec<_>>();
        let subscriptions = activity_subscriptions(&panes);
        let agents = subscriptions
            .iter()
            .filter(|subscription| subscription["type"] == "pane.agent_status_changed")
            .collect::<Vec<_>>();

        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0]["pane_id"], "w1:p1");
        assert_eq!(agents[1]["pane_id"], "w2:p3");
    }

    #[tokio::test]
    #[ignore = "requires permission to create a local Herdr Unix socket"]
    async fn isolated_herdr_socket_satisfies_the_read_write_contract() {
        let fake = FakeHerdr::start();
        let backend = HerdrBackend::new(&fake.socket_path);

        assert_eq!(backend.metadata().await.unwrap().protocol, Some(17));
        assert_eq!(backend.list_workspaces().await.unwrap().len(), 1);
        assert_eq!(backend.list_tabs().await.unwrap().len(), 1);
        assert_eq!(
            backend.list_panes().await.unwrap()[0]
                .terminal_id
                .as_deref(),
            Some("terminal-1")
        );
        let mut activity = backend.activity_stream().await.unwrap();
        let subscribed = tokio::time::timeout(std::time::Duration::from_secs(1), activity.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(subscribed.payload.get("result").is_some());
        assert_eq!(
            backend.list_agents().await.unwrap()[0].state_change_seq,
            Some(7)
        );
        assert_eq!(
            backend
                .read_pane(&ReadPane {
                    pane_id: PaneId::new("p1"),
                    source: OutputSource::RecentUnwrapped,
                    format: OutputFormat::Text,
                    lines: 200,
                    start: None,
                    end: None,
                })
                .await
                .unwrap()
                .text,
            "contract output"
        );

        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(PathBuf::from("/work/task")),
                label: Some("task".into()),
                focus: false,
            })
            .await
            .unwrap();
        let tab = backend
            .create_tab(&CreateTab {
                workspace_id: Some(workspace.id),
                cwd: None,
                label: None,
                focus: false,
            })
            .await
            .unwrap();
        let pane = backend
            .split_pane(&SplitPane {
                pane_id: PaneId::new("p1"),
                direction: SplitDirection::Down,
                ratio: Some(0.5),
                cwd: None,
                env: None,
            })
            .await
            .unwrap();
        backend
            .send_text(&pane.id, "hello", SendTextMode::Paste)
            .await
            .unwrap();
        backend
            .send_keys(&pane.id, &["Enter".into()])
            .await
            .unwrap();
        backend.focus_tab(&tab.id).await.unwrap();
        backend.prompt_agent("p1", "review").await.unwrap();

        let worktrees = backend
            .list_worktrees(&PathBuf::from("/work"))
            .await
            .unwrap();
        assert_eq!(worktrees[0].branch.as_deref(), Some("refs/heads/task"));
        let placement = backend
            .create_worktree(&WorktreeRequest {
                cwd: PathBuf::from("/work"),
                branch: "task".into(),
                label: Some("task".into()),
                focus: false,
            })
            .await
            .unwrap();
        assert_eq!(placement.pane_id.as_str(), "p1");

        let calls = fake.calls.lock().unwrap();
        let split = calls
            .iter()
            .find(|call| call["method"] == "pane.split")
            .unwrap();
        assert_eq!(split["params"]["target_pane_id"], "p1");
        assert_eq!(split["params"]["direction"], "down");
    }

    /// A herdr that accepts the connection and then never answers used to
    /// hold this task and its file descriptor forever, and every request that
    /// landed on it did the same.
    #[tokio::test]
    async fn a_herdr_that_never_answers_is_given_up_on() {
        let socket_path = crate::short_test_socket("gw-herdr-mute");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        // Accepts, reads nothing, answers nothing, and -- crucially -- holds
        // the connection open, so the gateway sees neither EOF nor an error.
        let held = std::sync::Arc::new(Mutex::new(Vec::new()));
        let keep = std::sync::Arc::clone(&held);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                keep.lock().unwrap().push(stream);
            }
        });

        let backend = HerdrBackend::with_request_timeout(&socket_path, Duration::from_millis(250));
        let started = std::time::Instant::now();
        let refused = backend.list_panes().await;
        let waited = started.elapsed();

        task.abort();
        let _ = std::fs::remove_file(&socket_path);

        assert!(
            matches!(refused, Err(BackendError::Unavailable)),
            "a mute herdr should surface as an unavailable backend, got {refused:?}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "gave up only after {waited:?}; without the bound it never gives up at all"
        );
    }

    /// The bound is on the request path only. An event subscription is
    /// supposed to sit idle -- a terminal nobody is typing into produces
    /// nothing for hours -- so a read bound here would tear down a healthy
    /// stream on a quiet session and reconnect it forever.
    #[tokio::test]
    async fn a_quiet_event_stream_is_not_torn_down_for_being_quiet() {
        let socket_path = crate::short_test_socket("gw-herdr-idle");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        // Answers the setup calls, then holds the subscription connection open
        // and says nothing on it -- a session nobody is typing into.
        let held = std::sync::Arc::new(Mutex::new(Vec::new()));
        let keep = std::sync::Arc::clone(&held);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let method = request["method"].as_str().unwrap_or_default().to_owned();
                let mut stream = reader.into_inner();
                if method == "events.subscribe" {
                    keep.lock().unwrap().push(stream);
                    continue;
                }
                let response = json!({ "id": request["id"], "result": fake_result(&method) });
                stream
                    .write_all(response.to_string().as_bytes())
                    .await
                    .unwrap();
                stream.write_all(b"\n").await.unwrap();
            }
        });

        let backend = HerdrBackend::with_request_timeout(&socket_path, Duration::from_millis(250));
        let mut stream = backend.activity_stream().await.unwrap();

        // Nothing happens for three times the request timeout. The stream must
        // still be waiting, not have ended or errored.
        let quiet = tokio::time::timeout(Duration::from_millis(750), stream.next()).await;

        task.abort();
        let _ = std::fs::remove_file(&socket_path);

        assert!(
            quiet.is_err(),
            "the stream ended or errored while merely being idle: {:?}",
            quiet.ok().flatten()
        );
    }

    /// The setup call inside `activity_stream` is *not* exempt: it goes
    /// through `request_transport`, so a herdr that will not answer cannot
    /// hang the stream constructor either.
    #[tokio::test]
    async fn opening_a_stream_against_a_mute_herdr_still_gives_up() {
        let socket_path = crate::short_test_socket("gw-herdr-open");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let held = std::sync::Arc::new(Mutex::new(Vec::new()));
        let keep = std::sync::Arc::clone(&held);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                keep.lock().unwrap().push(stream);
            }
        });

        let backend = HerdrBackend::with_request_timeout(&socket_path, Duration::from_millis(250));
        let opened = tokio::time::timeout(Duration::from_secs(5), backend.activity_stream()).await;

        task.abort();
        let _ = std::fs::remove_file(&socket_path);

        let opened = opened.expect("activity_stream hung on a herdr that never answers");
        assert!(matches!(opened.err(), Some(BackendError::Unavailable)));
    }
}
