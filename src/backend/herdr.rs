use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(unix)]
use tokio::net::UnixStream;

use super::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, CreateTab, CreateWorkspace, OutputFormat, OutputSource, Pane,
    PaneId, PaneOutput, ReadPane, SplitDirection, SplitPane, StartAgent, StartedAgent, Tab, TabId,
    TerminalBackend, Workspace, WorkspaceId, Worktree, WorktreePlacement, WorktreeRequest,
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

pub struct HerdrBackend {
    socket_path: PathBuf,
    /// Overridable so a test can prove the timeout fires without waiting out
    /// the real one.
    request_timeout: Duration,
}

impl HerdrBackend {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            request_timeout: REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_request_timeout(socket_path: impl Into<PathBuf>, request_timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            request_timeout,
        }
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

impl TerminalBackend for HerdrBackend {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        Box::pin(async move {
            let response = self.request("ping", json!({})).await?;
            Ok(BackendMetadata {
                kind: BackendKind::Herdr,
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
            response
                .pointer("/result/panes")
                .and_then(Value::as_array)
                .ok_or(BackendError::InvalidResponse("pane list"))?
                .iter()
                .map(pane_from_json)
                .collect()
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
            pane_from_json(pane)
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
            pane_from_json(pane)
        })
    }

    fn send_text<'a>(&'a self, id: &'a PaneId, text: &'a str) -> BackendFuture<'a, ()> {
        self.command(
            "pane.send_text",
            json!({ "pane_id": id.as_str(), "text": text }),
        )
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
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

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let mut params = serde_json::Map::new();
            params.insert("name".into(), json!(request.kind));
            params.insert("kind".into(), json!(request.kind));
            params.insert("pane_id".into(), json!(request.pane_id.as_str()));
            params.insert("timeout_ms".into(), json!(request.timeout_ms));
            if !request.args.is_empty() {
                params.insert("args".into(), json!(request.args));
            }
            let response = self.request("agent.start", Value::Object(params)).await?;
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
            Ok(StartedAgent { argv })
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
        width: value
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        height: value
            .get("height")
            .and_then(Value::as_u64)
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
        // Herdr's own envelope has never carried this; nothing to read.
        alternate_on: None,
    })
}

fn agent_from_json(value: &Value) -> Result<Agent, BackendError> {
    let pane_id = required_string(value, "pane_id", "agent")?;
    Ok(Agent {
        target: value
            .get("target")
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
    match &error {
        BackendError::Refused { code, message }
            if code.as_deref().is_some_and(|code| {
                matches!(code, "method_not_found" | "unknown_method" | "-32601")
                    || (code == "invalid_request" && message.contains("unknown variant"))
            }) =>
        {
            BackendError::Unsupported("worktrees")
        }
        _ => error,
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
    use std::sync::{Arc, Mutex};

    use super::*;
    use tokio_stream::StreamExt as _;

    struct FakeHerdr {
        socket_path: PathBuf,
        calls: Arc<Mutex<Vec<Value>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeHerdr {
        fn start() -> Self {
            let socket_path = crate::short_test_socket("gw-herdr");
            let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap_or_default();
                    let result = fake_result(method);
                    recorded.lock().unwrap().push(request.clone());
                    let response = json!({ "id": request["id"], "result": result });
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
        backend.send_text(&pane.id, "hello").await.unwrap();
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
