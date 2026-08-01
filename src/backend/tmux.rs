use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{
    AgentStatus, BackendError, BackendFuture, BackendKind, BackendMetadata, CreateTab,
    CreateWorkspace, OutputFormat, OutputSource, Pane, PaneId, PaneOutput, ReadPane,
    SplitDirection, SplitPane, Tab, TabId, TerminalBackend, Workspace, WorkspaceId,
};

const FIELD_SEPARATOR: char = '\u{1f}';

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
            })
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
                        label: fields[1].clone(),
                        focused: parse_u32(&fields[2], "session")? > 0,
                        tab_count: Some(parse_u32(&fields[3], "session")?),
                        active_tab_id: non_empty(&fields[4]).map(TabId::new),
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

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            validate_tmux_id(request.pane_id.as_str(), '%', "pane")?;
            let mut args = vec!["capture-pane".to_owned(), "-p".to_owned()];
            if request.format == OutputFormat::Ansi {
                args.push("-e".to_owned());
            }
            if request.source == OutputSource::RecentUnwrapped {
                args.push("-J".to_owned());
            }
            if request.source != OutputSource::Visible {
                args.push("-S".to_owned());
                args.push(format!("-{}", request.lines));
            }
            args.push("-t".to_owned());
            args.push(request.pane_id.as_str().to_owned());
            let captured = self.output(&args).await?;
            let text = tail_lines(&captured, request.lines as usize);
            Ok(PaneOutput {
                revision: Some(text_revision(&text)),
                text,
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
}

fn pane_from_fields(fields: Vec<String>) -> Result<Pane, BackendError> {
    let command = non_empty(&fields[10]);
    Ok(Pane {
        workspace_id: WorkspaceId::new(&fields[0]),
        tab_id: TabId::new(&fields[1]),
        id: PaneId::new(&fields[2]),
        label: non_empty(&fields[3]),
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
        has_scrollback: parse_u32(&fields[11], "pane")? > 0,
    })
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
        assert!(pane.has_scrollback);
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

    #[tokio::test]
    #[ignore = "requires permission to create a local tmux Unix socket"]
    async fn isolated_tmux_server_satisfies_the_read_write_contract() {
        if Command::new("tmux").arg("-V").output().await.is_err() {
            return;
        }
        let socket =
            std::env::temp_dir().join(format!("gateway-test-{}.sock", uuid::Uuid::new_v4()));
        let backend = TmuxBackend::new(Some(socket));
        assert!(backend.list_workspaces().await.unwrap().is_empty());
        let workspace = backend
            .create_workspace(&CreateWorkspace {
                cwd: Some(std::env::temp_dir()),
                label: Some("gateway-contract".into()),
                focus: true,
            })
            .await
            .unwrap();

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
                pane_id: split.id,
                source: OutputSource::Visible,
                format: OutputFormat::Text,
                lines: 80,
            })
            .await
            .unwrap();
        assert!(output.text.contains("gateway_contract_probe"));

        backend.close_workspace(&workspace.id).await.unwrap();
    }
}
