use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, CreateTab, CreateWorkspace, OutputFormat, OutputSource, Pane,
    PaneId, PaneOutput, PaneRange, ReadPane, SplitDirection, SplitPane, StartAgent, StartedAgent,
    Tab, TabId, TerminalBackend, Workspace, WorkspaceId,
};

const FIELD_SEPARATOR: char = '\u{1f}';

/// Mirrors `MAX_OUTPUT_LINES` in main. The backend clamps too so a direct
/// caller cannot ask tmux for a hundred thousand rows.
const MAX_CAPTURE_LINES: u32 = 5000;

#[derive(Clone)]
pub struct TmuxBackend {
    binary: PathBuf,
    socket_path: Option<PathBuf>,
}

impl TmuxBackend {
    pub fn new(socket_path: Option<PathBuf>) -> Self {
        Self {
            binary: PathBuf::from("tmux"),
            socket_path,
        }
    }

    #[cfg(test)]
    fn with_binary(binary: impl Into<PathBuf>, socket_path: Option<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            socket_path,
        }
    }

    async fn output<I, S>(&self, args: I) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        if let Some(socket_path) = &self.socket_path {
            command.arg("-S").arg(socket_path);
        }
        command.args(args);
        let output = command
            .output()
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if !output.status.success() {
            return Err(BackendError::Refused {
                code: None,
                // tmux diagnostics name commands and targets, never pane
                // output. Clamp them before this crosses an adapter boundary.
                message: String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .chars()
                    .take(240)
                    .collect(),
            });
        }
        String::from_utf8(output.stdout).map_err(|_| BackendError::InvalidResponse("UTF-8 output"))
    }

    async fn output_with_stdin<I, S>(&self, args: I, input: &[u8]) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        if let Some(socket_path) = &self.socket_path {
            command.arg("-S").arg(socket_path);
        }
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| BackendError::Unavailable)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or(BackendError::InvalidResponse("tmux stdin"))?;
        stdin
            .write_all(input)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|_| BackendError::Unavailable)?;
        if !output.status.success() {
            return Err(refused(&output.stderr));
        }
        String::from_utf8(output.stdout).map_err(|_| BackendError::InvalidResponse("UTF-8 output"))
    }

    async fn list_output<I, S>(&self, args: I) -> Result<String, BackendError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        match self.output(args).await {
            Ok(output) => Ok(output),
            Err(BackendError::Refused { message, .. }) if means_no_tmux_server(&message) => {
                Ok(String::new())
            }
            Err(error) => Err(error),
        }
    }

    fn format(fields: &[&str]) -> String {
        fields.join(&FIELD_SEPARATOR.to_string())
    }

    /// `(history_size, pane_height)` for one pane.
    ///
    /// `list_panes` carries both, but a single read has no reason to query every
    /// pane on the server to learn about one of them.
    async fn pane_metrics(&self, pane: &PaneId) -> Result<(u32, u32), BackendError> {
        validate_tmux_id(pane.as_str(), '%', "pane")?;
        let output = self
            .output(&[
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                pane.as_str().to_owned(),
                "-F".to_owned(),
                "#{history_size}\t#{pane_height}".to_owned(),
            ])
            .await?;
        let line = output.lines().next().unwrap_or_default();
        let mut parts = line.split('\t');
        let history = parse_u32(parts.next().unwrap_or_default(), "pane")?;
        let height = parse_u32(parts.next().unwrap_or_default(), "pane")?;
        Ok((history, height))
    }
}

impl TerminalBackend for TmuxBackend {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        Box::pin(async move {
            let version = self.output(["-V"]).await?;
            Ok(BackendMetadata {
                kind: BackendKind::Tmux,
                version: Some(
                    version
                        .trim()
                        .strip_prefix("tmux ")
                        .unwrap_or(version.trim())
                        .to_owned(),
                ),
                protocol: None,
                compatibility_response: None,
            })
        })
    }

    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream> {
        let backend = self.clone();
        Box::pin(async move {
            let activity = async_stream::stream! {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(500));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut previous = None;
                loop {
                    interval.tick().await;
                    match (
                        backend.list_workspaces().await,
                        backend.list_tabs().await,
                        backend.list_panes().await,
                    ) {
                        (Ok(workspaces), Ok(tabs), Ok(panes)) => {
                            let fingerprint = format!("{workspaces:?}{tabs:?}{panes:?}");
                            let changed = previous.as_ref().is_some_and(|last| last != &fingerprint);
                            previous = Some(fingerprint);
                            if changed {
                                yield Ok(BackendActivity {
                                    name: "layout_updated".into(),
                                    payload: serde_json::json!({
                                        "event": "layout_updated",
                                        "data": { "backend": "tmux" }
                                    }),
                                });
                            }
                        }
                        _ => yield Err(BackendError::Unavailable),
                    }
                }
            };
            Ok(Box::pin(activity) as BackendActivityStream)
        })
    }

    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{session_name}",
                "#{session_attached}",
                "#{session_windows}",
                "#{window_id}",
            ]);
            let output = self.list_output(["list-sessions", "-F", &format]).await?;
            parse_rows(&output, 5, "session")?
                .into_iter()
                .map(|fields| {
                    Ok(Workspace {
                        id: WorkspaceId::new(&fields[0]),
                        number: None,
                        label: fields[1].clone(),
                        focused: parse_u32(&fields[2], "session")? > 0,
                        tab_count: Some(parse_u32(&fields[3], "session")?),
                        active_tab_id: non_empty(&fields[4]).map(TabId::new),
                        pane_count: None,
                        agent_status: AgentStatus::Unknown,
                        repo_root: None,
                        checkout_path: None,
                    })
                })
                .collect()
        })
    }

    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{window_id}",
                "#{window_name}",
                "#{window_active}",
                "#{pane_id}",
                "#{window_panes}",
            ]);
            let output = self
                .list_output(["list-windows", "-a", "-F", &format])
                .await?;
            parse_rows(&output, 6, "window")?
                .into_iter()
                .map(|fields| {
                    Ok(Tab {
                        workspace_id: WorkspaceId::new(&fields[0]),
                        id: TabId::new(&fields[1]),
                        label: fields[2].clone(),
                        focused: parse_flag(&fields[3], "window")?,
                        active_pane_id: non_empty(&fields[4]).map(PaneId::new),
                        pane_count: Some(parse_u32(&fields[5], "window")?),
                    })
                })
                .collect()
        })
    }

    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>> {
        Box::pin(async move {
            let format = Self::format(&[
                "#{session_id}",
                "#{window_id}",
                "#{pane_id}",
                "#{pane_title}",
                "#{pane_current_path}",
                "#{pane_active}",
                "#{window_active}",
                "#{session_attached}",
                "#{pane_width}",
                "#{pane_height}",
                "#{pane_current_command}",
                "#{history_size}",
            ]);
            let output = self
                .list_output(["list-panes", "-a", "-F", &format])
                .await?;
            parse_rows(&output, 12, "pane")?
                .into_iter()
                .map(pane_from_fields)
                .collect()
        })
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>> {
        Box::pin(async move {
            Ok(self
                .list_panes()
                .await?
                .into_iter()
                .filter_map(agent_from_pane)
                .collect())
        })
    }

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.list_panes()
                .await?
                .into_iter()
                .find(|pane| pane.id == *id)
                .ok_or(BackendError::InvalidTarget("pane"))
        })
    }

    fn get_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, Agent> {
        Box::pin(async move {
            let pane = self.get_pane(&PaneId::new(target)).await?;
            agent_from_pane(pane).ok_or(BackendError::InvalidTarget("agent"))
        })
    }

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;
            let (history_size, pane_height) = self.pane_metrics(&request.pane_id).await?;
            let total = history_size.saturating_add(pane_height);

            let mut args = vec!["capture-pane".to_owned(), "-p".to_owned()];
            if request.format == OutputFormat::Ansi {
                args.push("-e".to_owned());
            }
            if request.source == OutputSource::RecentUnwrapped {
                args.push("-J".to_owned());
            }

            let range = match (request.start, request.end) {
                // `visible` is the live screen, which no range addresses.
                (Some(start), Some(end)) if request.source != OutputSource::Visible => {
                    let start = start.min(total.saturating_sub(1));
                    let end = end.min(total).max(start + 1);
                    let end = end.min(start + MAX_CAPTURE_LINES);
                    let (s, e) = capture_bounds(start, end, history_size);
                    args.push("-S".to_owned());
                    args.push(s.to_string());
                    args.push("-E".to_owned());
                    args.push(e.to_string());
                    Some(PaneRange { start, end, total })
                }
                _ => {
                    if request.source != OutputSource::Visible {
                        args.push("-S".to_owned());
                        args.push(format!("-{}", request.lines));
                    }
                    None
                }
            };

            args.push("-t".to_owned());
            args.push(request.pane_id.as_str().to_owned());
            let captured = self.output(&args).await?;

            let (text, range) = match range {
                Some(range) => (captured, Some(range)),
                None => {
                    let text = tail_lines(&captured, request.lines as usize);
                    let served = text.lines().count() as u32;
                    let range = PaneRange {
                        start: total.saturating_sub(served),
                        end: total,
                        total,
                    };
                    (text, Some(range))
                }
            };

            Ok(PaneOutput {
                revision: Some(text_revision(&text)),
                text,
                range,
            })
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            let mut args = vec![
                "new-session".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{session_id}".to_owned(),
            ];
            if let Some(label) = request.label.as_deref() {
                args.extend(["-s".to_owned(), label.to_owned()]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '$', "workspace")?;
            let id = WorkspaceId::new(raw_id);
            if request.focus {
                self.focus_workspace(&id).await?;
            }
            self.list_workspaces()
                .await?
                .into_iter()
                .find(|workspace| workspace.id == id)
                .ok_or(BackendError::InvalidResponse("created workspace"))
        })
    }

    fn focus_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            let clients = self
                .output(["list-clients", "-F", "#{client_name}"])
                .await?;
            for client in clients.lines().filter(|client| !client.is_empty()) {
                self.output(["switch-client", "-c", client, "-t", id.as_str()])
                    .await?;
            }
            Ok(())
        })
    }

    fn rename_workspace<'a>(
        &'a self,
        id: &'a WorkspaceId,
        label: &'a str,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            self.output(["rename-session", "-t", id.as_str(), label])
                .await?;
            Ok(())
        })
    }

    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '$', "workspace")?;
            self.output(["kill-session", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab> {
        Box::pin(async move {
            let workspace_id = match &request.workspace_id {
                Some(id) => id.clone(),
                None => {
                    let workspaces = self.list_workspaces().await?;
                    workspaces
                        .iter()
                        .find(|workspace| workspace.focused)
                        .or_else(|| workspaces.first())
                        .map(|workspace| workspace.id.clone())
                        .ok_or(BackendError::InvalidTarget("workspace"))?
                }
            };
            validate_tmux_id(workspace_id.as_str(), '$', "workspace")?;
            let mut args = vec![
                "new-window".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{window_id}".to_owned(),
                "-t".to_owned(),
                workspace_id.as_str().to_owned(),
            ];
            if let Some(label) = request.label.as_deref() {
                args.extend(["-n".to_owned(), label.to_owned()]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '@', "tab")?;
            let id = TabId::new(raw_id);
            if request.focus {
                self.focus_tab(&id).await?;
            }
            self.list_tabs()
                .await?
                .into_iter()
                .find(|tab| tab.id == id)
                .ok_or(BackendError::InvalidResponse("created tab"))
        })
    }

    fn focus_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["select-window", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn rename_tab<'a>(&'a self, id: &'a TabId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["rename-window", "-t", id.as_str(), label])
                .await?;
            Ok(())
        })
    }

    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '@', "tab")?;
            self.output(["kill-window", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn focus_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["select-pane", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn rename_pane<'a>(&'a self, id: &'a PaneId, label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["select-pane", "-t", id.as_str(), "-T", label])
                .await?;
            Ok(())
        })
    }

    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            self.output(["kill-pane", "-t", id.as_str()]).await?;
            Ok(())
        })
    }

    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;
            let mut args = vec![
                "split-window".to_owned(),
                "-d".to_owned(),
                "-P".to_owned(),
                "-F".to_owned(),
                "#{pane_id}".to_owned(),
                "-t".to_owned(),
                request.pane_id.as_str().to_owned(),
                match request.direction {
                    SplitDirection::Right => "-h".to_owned(),
                    SplitDirection::Down => "-v".to_owned(),
                },
            ];
            if let Some(ratio) = request.ratio {
                if !(0.05..=0.95).contains(&ratio) {
                    return Err(BackendError::InvalidResponse("split ratio"));
                }
                args.extend([
                    "-p".to_owned(),
                    ((ratio * 100.0).round() as u32).to_string(),
                ]);
            }
            if let Some(cwd) = &request.cwd {
                args.extend(["-c".to_owned(), cwd.to_string_lossy().into_owned()]);
            }
            let raw_id = self.output(&args).await?.trim().to_owned();
            validate_tmux_id(&raw_id, '%', "pane")?;
            self.get_pane(&PaneId::new(raw_id)).await
        })
    }

    fn send_text<'a>(&'a self, id: &'a PaneId, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            let buffer = format!("gateway-{}", uuid::Uuid::new_v4());
            self.output_with_stdin(["load-buffer", "-b", &buffer, "-"], text.as_bytes())
                .await?;
            let pasted = self
                .output(["paste-buffer", "-b", &buffer, "-t", id.as_str(), "-d"])
                .await;
            if pasted.is_err() {
                let _ = self.output(["delete-buffer", "-b", &buffer]).await;
            }
            pasted.map(|_| ())
        })
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            validate_tmux_id(id.as_str(), '%', "pane")?;
            let mapped = keys
                .iter()
                .map(|key| tmux_key(key))
                .collect::<Result<Vec<_>, _>>()?;
            let mut args = vec![
                "send-keys".to_owned(),
                "-t".to_owned(),
                id.as_str().to_owned(),
            ];
            args.extend(mapped);
            self.output(&args).await?;
            Ok(())
        })
    }

    fn focus_agent<'a>(&'a self, target: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.focus_pane(&PaneId::new(target)).await })
    }

    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.send_text(&PaneId::new(target), text).await })
    }

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let executable = request
                .executable
                .as_ref()
                .ok_or_else(|| BackendError::Refused {
                    code: Some("agent_not_found".into()),
                    message: format!("agent executable {} was not found on PATH", request.command),
                })?;
            let mut argv = vec![executable.to_string_lossy().into_owned()];
            argv.extend(request.args.iter().cloned());
            let command_line = argv
                .iter()
                .map(|argument| shell_word(argument))
                .collect::<Vec<_>>()
                .join(" ");
            self.send_text(&request.pane_id, &command_line).await?;
            self.send_keys(&request.pane_id, &["Enter".to_owned()])
                .await?;

            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(request.timeout_ms);
            let executable_name = executable
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&request.command);
            loop {
                if let Ok(current) = self.get_pane(&request.pane_id).await {
                    let recognized = current.agent.as_deref() == Some(request.kind.as_str())
                        || current.foreground_command.as_deref() == Some(executable_name);
                    if recognized {
                        return Ok(StartedAgent { argv: Some(argv) });
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(BackendError::Refused {
                        code: Some("agent_start_timeout".into()),
                        message: format!(
                            "agent {} did not become visible before timeout",
                            request.kind
                        ),
                    });
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
    }
}

fn pane_from_fields(fields: Vec<String>) -> Result<Pane, BackendError> {
    let command = non_empty(&fields[10]);
    Ok(Pane {
        workspace_id: WorkspaceId::new(&fields[0]),
        tab_id: TabId::new(&fields[1]),
        id: PaneId::new(&fields[2]),
        terminal_id: Some(fields[2].clone()),
        label: non_empty(&fields[3]),
        terminal_title: non_empty(&fields[3]),
        cwd: non_empty(&fields[4]).map(PathBuf::from),
        focused: parse_flag(&fields[5], "pane")?
            && parse_flag(&fields[6], "pane")?
            && parse_u32(&fields[7], "pane")? > 0,
        width: Some(parse_u32(&fields[8], "pane")?),
        height: Some(parse_u32(&fields[9], "pane")?),
        revision: None,
        agent: detected_agent(command.as_deref()),
        foreground_command: command,
        agent_status: AgentStatus::Unknown,
        max_offset_from_bottom: Some(parse_u32(&fields[11], "pane")?),
        viewport_rows: Some(parse_u32(&fields[9], "pane")?),
    })
}

fn agent_from_pane(pane: Pane) -> Option<Agent> {
    let kind = pane.agent.clone()?;
    Some(Agent {
        target: pane.id.as_str().to_owned(),
        pane_id: pane.id,
        workspace_id: Some(pane.workspace_id),
        tab_id: Some(pane.tab_id),
        display_agent: Some(kind.clone()),
        kind: Some(kind),
        status: pane.agent_status,
        state_change_seq: None,
    })
}

/// Quote one argv value for the interactive shell running in a tmux pane.
/// Values are data: they are always single-quoted and never interpreted as
/// shell structure assembled by the adapter.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn refused(stderr: &[u8]) -> BackendError {
    BackendError::Refused {
        code: None,
        message: String::from_utf8_lossy(stderr)
            .trim()
            .chars()
            .take(240)
            .collect(),
    }
}

fn means_no_tmux_server(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("no server running")
        || message.contains("failed to connect to server")
        || message.contains("no such file or directory")
}

fn tmux_key(value: &str) -> Result<String, BackendError> {
    let lower = value.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "enter" => "Enter".to_owned(),
        "escape" | "esc" => "Escape".to_owned(),
        "tab" => "Tab".to_owned(),
        "backspace" => "BSpace".to_owned(),
        "up" | "arrowup" => "Up".to_owned(),
        "down" | "arrowdown" => "Down".to_owned(),
        "left" | "arrowleft" => "Left".to_owned(),
        "right" | "arrowright" => "Right".to_owned(),
        _ if lower.starts_with("ctrl+") && lower.len() == 6 => {
            let key = lower.as_bytes()[5] as char;
            if !key.is_ascii_alphabetic() {
                return Err(BackendError::InvalidResponse("key name"));
            }
            format!("C-{key}")
        }
        _ if value.chars().count() == 1 && !value.chars().any(char::is_control) => value.to_owned(),
        _ => return Err(BackendError::InvalidResponse("key name")),
    };
    Ok(mapped)
}

fn parse_rows(
    output: &str,
    expected_fields: usize,
    context: &'static str,
) -> Result<Vec<Vec<String>>, BackendError> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields = line
                .split(FIELD_SEPARATOR)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            if fields.len() != expected_fields {
                return Err(BackendError::InvalidResponse(context));
            }
            Ok(fields)
        })
        .collect()
}

fn parse_flag(value: &str, context: &'static str) -> Result<bool, BackendError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(BackendError::InvalidResponse(context)),
    }
}

fn parse_u32(value: &str, context: &'static str) -> Result<u32, BackendError> {
    value
        .parse()
        .map_err(|_| BackendError::InvalidResponse(context))
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn validate_tmux_id(value: &str, prefix: char, kind: &'static str) -> Result<(), BackendError> {
    let mut chars = value.chars();
    if chars.next() != Some(prefix) || !chars.clone().all(|character| character.is_ascii_digit()) {
        return Err(BackendError::InvalidTarget(kind));
    }
    if chars.next().is_none() {
        return Err(BackendError::InvalidTarget(kind));
    }
    Ok(())
}

/// Absolute `[start, end)` to the inclusive `-S`/`-E` pair tmux wants.
///
/// tmux numbers rows from the top of the *visible* pane: 0 is the first visible
/// row and negatives reach back into scrollback, so absolute line `i` sits at
/// `i - history_size`. `-E` is inclusive, hence the extra `- 1`.
fn capture_bounds(start: u32, end: u32, history_size: u32) -> (i64, i64) {
    let origin = i64::from(history_size);
    (
        i64::from(start) - origin,
        i64::from(end.max(start + 1)) - 1 - origin,
    )
}

fn tail_lines(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

fn text_revision(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn detected_agent(command: Option<&str>) -> Option<String> {
    const AGENTS: &[&str] = &[
        "claude", "codex", "opencode", "qodercli", "gemini", "amp", "cursor", "pi",
    ];
    let command = command?.rsplit('/').next()?.to_ascii_lowercase();
    AGENTS
        .iter()
        .find(|agent| command == **agent || command.starts_with(&format!("{agent}-")))
        .map(|agent| (*agent).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt as _;

    #[test]
    fn parses_tmux_panes_into_backend_neutral_entities() {
        let row = [
            "$0",
            "@2",
            "%9",
            "agent",
            "/work/project",
            "1",
            "1",
            "1",
            "122",
            "41",
            "claude",
            "12",
        ]
        .join(&FIELD_SEPARATOR.to_string());
        let fields = parse_rows(&row, 12, "pane").unwrap().remove(0);
        let pane = pane_from_fields(fields).unwrap();

        assert_eq!(pane.workspace_id.as_str(), "$0");
        assert_eq!(pane.tab_id.as_str(), "@2");
        assert_eq!(pane.id.as_str(), "%9");
        assert_eq!(pane.agent.as_deref(), Some("claude"));
        assert_eq!(pane.cwd, Some(PathBuf::from("/work/project")));
        assert_eq!(pane.max_offset_from_bottom, Some(12));
        assert_eq!(pane.viewport_rows, Some(41));
    }

    #[test]
    fn target_ids_are_type_checked_before_reaching_tmux() {
        assert!(validate_tmux_id("%9", '%', "pane").is_ok());
        assert!(validate_tmux_id("@9", '%', "pane").is_err());
        assert!(validate_tmux_id("%9;kill-server", '%', "pane").is_err());
        assert!(validate_tmux_id("%", '%', "pane").is_err());
    }

    #[test]
    fn mobile_key_names_are_mapped_without_becoming_tmux_options() {
        assert_eq!(tmux_key("Enter").unwrap(), "Enter");
        assert_eq!(tmux_key("ctrl+c").unwrap(), "C-c");
        assert_eq!(tmux_key("ArrowUp").unwrap(), "Up");
        assert!(tmux_key("-t").is_err());
        assert!(tmux_key("C-x;kill-server").is_err());
    }

    #[test]
    fn capture_is_trimmed_to_the_requested_rows() {
        assert_eq!(tail_lines("one\ntwo\nthree\nfour\n", 2), "three\nfour");
        assert_eq!(tail_lines("one\ntwo", 10), "one\ntwo");
        assert_eq!(tail_lines("one", 0), "");
    }

    #[test]
    fn agent_detection_is_conservative() {
        assert_eq!(detected_agent(Some("claude")), Some("claude".into()));
        assert_eq!(detected_agent(Some("claude-canary")), Some("claude".into()));
        assert_eq!(detected_agent(Some("bash")), None);
        assert_eq!(detected_agent(Some("notcodex")), None);
    }

    #[test]
    fn agent_arguments_are_shell_quoted_as_single_words() {
        assert_eq!(shell_word("plain"), "'plain'");
        assert_eq!(shell_word("two words"), "'two words'");
        assert_eq!(shell_word("a'b;$(touch nope)"), "'a'\\''b;$(touch nope)'");
    }

    #[test]
    fn an_absent_tmux_server_is_an_empty_topology() {
        assert!(means_no_tmux_server(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(means_no_tmux_server(
            "error connecting to /tmp/missing.sock (No such file or directory)"
        ));
        assert!(!means_no_tmux_server("can't find pane: %99"));
    }

    #[test]
    fn command_runner_never_needs_a_shell() {
        let backend = TmuxBackend::with_binary("tmux", Some(PathBuf::from("/tmp/tmux.sock")));
        assert_eq!(backend.binary, PathBuf::from("tmux"));
        assert_eq!(backend.socket_path, Some(PathBuf::from("/tmp/tmux.sock")));
    }

    #[test]
    fn capture_bounds_map_absolute_lines_onto_tmux_coordinates() {
        // Measured live: history_size 463, pane_height 41, total 504.
        // tmux counts from the top of the visible pane, and -E is inclusive.
        assert_eq!(capture_bounds(0, 504, 463), (-463, 40));
        // The oldest line alone.
        assert_eq!(capture_bounds(0, 1, 463), (-463, -463));
        // The first visible row alone.
        assert_eq!(capture_bounds(463, 464, 463), (0, 0));
        // A pane with no scrollback at all.
        assert_eq!(capture_bounds(0, 41, 0), (0, 40));
    }

    /// Fresh backend on a private tmux socket, isolated from any real server.
    fn fresh_backend() -> TmuxBackend {
        let socket =
            std::env::temp_dir().join(format!("gateway-test-{}.sock", uuid::Uuid::new_v4()));
        TmuxBackend::new(Some(socket))
    }

    /// A fresh backend with one workspace already created on it, for contract
    /// tests that just need somewhere live to read from.
    async fn contract_workspace() -> (TmuxBackend, Workspace) {
        let backend = fresh_backend();
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-contract".into()),
                focus: true,
            })
            .await
            .unwrap();
        (backend, workspace)
    }

    #[tokio::test]
    #[ignore = "requires a tmux server"]
    async fn pane_metrics_report_history_and_height() {
        let (backend, workspace) = contract_workspace().await;
        let panes = backend.list_panes().await.unwrap();
        let pane = panes
            .iter()
            .find(|p| p.workspace_id == workspace.id)
            .unwrap();
        let (history, height) = backend.pane_metrics(&pane.id).await.unwrap();
        assert_eq!(Some(height), pane.viewport_rows);
        assert_eq!(Some(history), pane.max_offset_from_bottom);
        backend.close_workspace(&workspace.id).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn isolated_tmux_server_satisfies_the_read_write_contract() {
        if Command::new("tmux").arg("-V").output().await.is_err() {
            return;
        }
        let backend = fresh_backend();
        assert!(backend.list_workspaces().await.unwrap().is_empty());
        let mut activity = backend.activity_stream().await.unwrap();
        let activity_change = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(2), activity.next()).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-contract".into()),
                focus: true,
            })
            .await
            .unwrap();
        let event = activity_change.await.unwrap().unwrap().unwrap().unwrap();
        assert_eq!(event.name, "layout_updated");

        let initial_pane = backend
            .list_panes()
            .await
            .unwrap()
            .into_iter()
            .find(|pane| pane.workspace_id == workspace.id)
            .unwrap();
        let tab = backend
            .create_tab(&CreateTab {
                workspace_id: None,
                cwd: Some(std::env::temp_dir()),
                label: Some("second".into()),
                focus: false,
            })
            .await
            .unwrap();
        assert_eq!(tab.workspace_id, workspace.id);

        let split = backend
            .split_pane(&SplitPane {
                pane_id: initial_pane.id,
                direction: SplitDirection::Right,
                ratio: Some(0.5),
                cwd: Some(std::env::temp_dir()),
                env: None,
            })
            .await
            .unwrap();
        backend
            .send_text(&split.id, "printf gateway_contract_probe")
            .await
            .unwrap();
        backend
            .send_keys(&split.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let output = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::Visible,
                format: OutputFormat::Text,
                lines: 80,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(output.text.contains("gateway_contract_probe"));

        backend.send_text(&split.id, "seq 1 800").await.unwrap();
        backend
            .send_keys(&split.id, &["Enter".to_owned()])
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let first_page = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 240,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        let third_page = backend
            .read_pane(&ReadPane {
                pane_id: split.id.clone(),
                source: OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 720,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(first_page.text.lines().count(), 240);
        assert_eq!(third_page.text.lines().count(), 720);
        assert!(third_page.text.ends_with(&first_page.text));

        backend.close_workspace(&workspace.id).await.unwrap();
    }
}
