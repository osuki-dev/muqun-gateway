use std::path::PathBuf;

use anyhow::Context as _;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(unix)]
use tokio::net::UnixStream;

use super::{
    AgentStatus, BackendError, BackendFuture, BackendKind, BackendMetadata, CreateTab,
    CreateWorkspace, OutputFormat, OutputSource, Pane, PaneId, PaneOutput, ReadPane,
    SplitDirection, SplitPane, Tab, TabId, TerminalBackend, Workspace, WorkspaceId,
};

pub struct HerdrBackend {
    socket_path: PathBuf,
}

impl HerdrBackend {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        let response = self
            .request_transport(method, params)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if let Some(error) = response.get("error") {
            return Err(BackendError::Refused {
                code: error.get("code").and_then(Value::as_str).map(str::to_owned),
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

        #[cfg(not(unix))]
        {
            let _ = (method, params);
            anyhow::bail!("Herdr socket transport is unavailable on this platform")
        }
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
            })
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

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let response = self
                .request("pane.get", json!({ "pane_id": id.as_str() }))
                .await?;
            let pane = response.pointer("/result/pane").unwrap_or(&response);
            pane_from_json(pane)
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
                text: text.to_owned(),
                revision,
            })
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            let response = self
                .request(
                    "workspace.create",
                    json!({
                        "cwd": request.cwd,
                        "label": request.label,
                        "focus": request.focus,
                    }),
                )
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
            let response = self
                .request(
                    "tab.create",
                    json!({
                        "workspace_id": request.workspace_id.as_ref().map(WorkspaceId::as_str),
                        "cwd": request.cwd,
                        "label": request.label,
                        "focus": request.focus,
                    }),
                )
                .await?;
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
            let response = self
                .request(
                    "pane.split",
                    json!({
                        "target_pane_id": request.pane_id.as_str(),
                        "direction": match request.direction {
                            SplitDirection::Right => "right",
                            SplitDirection::Down => "down",
                        },
                        "ratio": request.ratio,
                        "cwd": request.cwd,
                    }),
                )
                .await?;
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
        workspace_id: WorkspaceId::new(required_string(value, "workspace_id", "pane")?),
        tab_id: TabId::new(required_string(value, "tab_id", "pane")?),
        label: value
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: value
            .get("cwd")
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
        has_scrollback: value
            .pointer("/scroll/max_offset_from_bottom")
            .and_then(Value::as_u64)
            .is_some_and(|offset| offset > 0),
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
