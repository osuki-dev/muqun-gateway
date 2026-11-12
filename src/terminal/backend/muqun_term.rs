//! Native `muqun-desktop` persistent terminal daemon (`term.sock`) adapter.
//!
//! Speaks the newline-delimited JSON protocol of `muqun-desktop --term-daemon`
//! over a Unix domain socket and maps its flat session list onto the
//! backend-neutral workspace/tab/pane hierarchy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;

use super::keys::{unknown_key, KeyBase, KeyChord, KeyboardVocabulary, NamedKey};
use super::{
    Agent, AgentStatus, BackendActivity, BackendActivityStream, BackendError, BackendFuture,
    BackendKind, BackendMetadata, CreateTab, CreateWorkspace, OutputFormat, Pane, PaneId,
    PaneOutput, ReadPane, SendTextMode, SplitPane, StartAgent, StartedAgent, Tab, TabId,
    TerminalBackend, Workspace, WorkspaceId,
};
use crate::terminal::shell_word::shell_word;

pub const MUQUN_WORKSPACE_ID: &str = "muqun-workspace";
pub const MUQUN_WORKSPACE_LABEL: &str = "Muqun Terminal";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const ACTIVITY_POLL_MIN: Duration = Duration::from_millis(500);
const ACTIVITY_POLL_MAX: Duration = Duration::from_secs(2);
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Resolves the `muqun-desktop` terminal daemon socket path.
///
/// Honors `MUQUN_TERM_SOCKET_PATH` when set and non-empty; otherwise defaults
/// to `<config_dir>/muqun-desktop/term.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("MUQUN_TERM_SOCKET_PATH") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("muqun-desktop")
        .join("term.sock")
}

#[derive(Debug, Clone, Deserialize)]
struct DaemonSession {
    id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default = "default_alive")]
    alive: bool,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    shell_pid: Option<u32>,
    #[serde(default)]
    agent_status: Option<String>,
    #[serde(default)]
    task_summary: Option<String>,
}

fn default_alive() -> bool {
    true
}

fn default_cols() -> u16 {
    DEFAULT_COLS
}

fn default_rows() -> u16 {
    DEFAULT_ROWS
}

#[derive(Debug, Clone)]
pub struct MuqunTermBackend {
    socket_path: PathBuf,
}

impl MuqunTermBackend {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    async fn request(&self, payload: Value) -> Result<Value, BackendError> {
        let response = self
            .request_transport(payload)
            .await
            .map_err(|_| BackendError::Unavailable)?;
        let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(true);
        if !ok || response.get("error").is_some() {
            let message = response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("request refused")
                .to_owned();
            let lower = message.to_ascii_lowercase();
            if lower.contains("not found") || lower.contains("no such session") {
                return Err(BackendError::InvalidTarget("pane"));
            }
            return Err(BackendError::Refused {
                code: None,
                message,
            });
        }
        Ok(response)
    }

    async fn request_transport(&self, payload: Value) -> anyhow::Result<Value> {
        #[cfg(unix)]
        {
            tokio::time::timeout(REQUEST_TIMEOUT, self.exchange(payload))
                .await
                .with_context(|| {
                    format!(
                        "muqun-term daemon did not answer within {}s",
                        REQUEST_TIMEOUT.as_secs()
                    )
                })?
        }

        #[cfg(not(unix))]
        {
            let _ = payload;
            anyhow::bail!("muqun-term unix socket transport is unavailable on this platform")
        }
    }

    #[cfg(unix)]
    async fn exchange(&self, payload: Value) -> anyhow::Result<Value> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| format!("failed to connect {}", self.socket_path.display()))?;
        let mut raw = serde_json::to_vec(&payload)?;
        raw.push(b'\n');
        stream.write_all(&raw).await?;
        stream.flush().await?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        anyhow::ensure!(!line.trim().is_empty(), "empty response from term.sock");
        Ok(serde_json::from_str(line.trim())?)
    }

    async fn list_alive_sessions(&self) -> Result<Vec<DaemonSession>, BackendError> {
        let response = self.request(json!({ "cmd": "list" })).await?;
        let sessions_val = response
            .get("sessions")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let sessions: Vec<DaemonSession> = serde_json::from_value(sessions_val)
            .map_err(|_| BackendError::InvalidResponse("session list"))?;
        Ok(sessions.into_iter().filter(|s| s.alive).collect())
    }

    async fn create_session(
        &self,
        cwd: Option<&Path>,
        label: Option<&str>,
    ) -> Result<String, BackendError> {
        let cwd_str = cwd.map(|p| p.to_string_lossy().into_owned());
        let response = self
            .request(json!({
                "cmd": "create",
                "id": Value::Null,
                "cwd": cwd_str,
                "cols": DEFAULT_COLS,
                "rows": DEFAULT_ROWS,
                "label": label,
                "command": Value::Null,
            }))
            .await?;
        response
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or(BackendError::InvalidResponse("created session id"))
    }

    async fn send_input_bytes(&self, id: &str, bytes: &[u8]) -> Result<(), BackendError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        self.request(json!({
            "cmd": "input",
            "id": id,
            "data": encoded,
        }))
        .await?;
        Ok(())
    }

    async fn topology_fingerprint(&self) -> Result<String, BackendError> {
        let sessions = self.list_alive_sessions().await?;
        let mut out = String::new();
        for s in sessions {
            out.push_str(&format!(
                "{}|{}|{}x{}|{}|{}\n",
                s.id,
                s.cwd,
                s.cols,
                s.rows,
                s.label.as_deref().unwrap_or(""),
                s.agent_status.as_deref().unwrap_or(""),
            ));
        }
        Ok(out)
    }
}

fn pane_from_session(s: &DaemonSession, focused: bool) -> Pane {
    Pane {
        history_identity: None,
        id: PaneId::new(&s.id),
        terminal_id: Some(s.id.clone()),
        workspace_id: WorkspaceId::new(MUQUN_WORKSPACE_ID),
        tab_id: TabId::new(&s.id),
        label: s.label.clone().or_else(|| Some(s.id.clone())),
        terminal_title: s.task_summary.clone().or_else(|| s.label.clone()),
        cwd: Some(PathBuf::from(&s.cwd)),
        focused,
        width: Some(u32::from(s.cols)),
        height: Some(u32::from(s.rows)),
        revision: None,
        foreground_command: None,
        agent: None,
        agent_status: AgentStatus::parse(s.agent_status.as_deref()),
        max_offset_from_bottom: None,
        viewport_rows: Some(u32::from(s.rows)),
        alternate_on: None,
        cursor_x: None,
        cursor_y: None,
        shell_pid: s.shell_pid,
    }
}

fn agent_from_pane(pane: Pane) -> Agent {
    let kind = pane.agent.clone().or_else(|| Some("shell".to_owned()));
    Agent {
        conversation_id: None,
        instance_id: Some(pane.id.as_str().to_owned()),
        target: pane.id.as_str().to_owned(),
        pane_id: pane.id,
        workspace_id: Some(pane.workspace_id),
        tab_id: Some(pane.tab_id),
        display_agent: kind.clone(),
        kind,
        status: pane.agent_status,
        state_change_seq: None,
        shell_pid: pane.shell_pid,
        pending_user_input: None,
    }
}

/// Strips ANSI escape sequences (CSI, OSC, and two-byte escapes) when plain
/// text output is requested.
fn strip_ansi_codes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek().copied() == Some('\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if ch != '\r' {
            out.push(ch);
        }
    }
    out
}

fn tail_lines(text: &str, max_lines: u32) -> String {
    let max = max_lines as usize;
    if max == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max);
    lines[start..].join("\n")
}

/// Translates a parsed [`KeyChord`] into the terminal byte sequence expected by
/// a PTY shell or TUI application.
fn chord_to_bytes(chord: &KeyChord) -> Vec<u8> {
    if let Some(seq) = chord.editing_sequence() {
        return seq.into_bytes();
    }
    let modifier = 1 + u8::from(chord.shift) + 2 * u8::from(chord.alt) + 4 * u8::from(chord.ctrl);
    match chord.base {
        KeyBase::Named(NamedKey::Enter) => {
            if chord.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        }
        KeyBase::Named(NamedKey::Esc) => b"\x1b".to_vec(),
        KeyBase::Named(NamedKey::Tab) => {
            if chord.shift && !chord.ctrl && !chord.alt {
                b"\x1b[Z".to_vec()
            } else if chord.alt {
                b"\x1b\t".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        KeyBase::Named(NamedKey::Backspace) => {
            if chord.alt {
                b"\x1b\x7f".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        }
        KeyBase::Named(NamedKey::Space) => {
            let byte = if chord.ctrl { b'\x00' } else { b' ' };
            if chord.alt {
                vec![b'\x1b', byte]
            } else {
                vec![byte]
            }
        }
        KeyBase::Named(
            key @ (NamedKey::Up | NamedKey::Down | NamedKey::Left | NamedKey::Right),
        ) => {
            let letter = match key {
                NamedKey::Up => 'A',
                NamedKey::Down => 'B',
                NamedKey::Right => 'C',
                NamedKey::Left => 'D',
                _ => unreachable!(),
            };
            if modifier == 1 {
                format!("\x1b[{letter}").into_bytes()
            } else {
                format!("\x1b[1;{modifier}{letter}").into_bytes()
            }
        }
        KeyBase::Named(NamedKey::F(num)) => match num {
            1..=4 => {
                let letter = match num {
                    1 => 'P',
                    2 => 'Q',
                    3 => 'R',
                    4 => 'S',
                    _ => unreachable!(),
                };
                if modifier == 1 {
                    format!("\x1bO{letter}").into_bytes()
                } else {
                    format!("\x1b[1;{modifier}{letter}").into_bytes()
                }
            }
            5..=12 => {
                let code = match num {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    12 => 24,
                    _ => unreachable!(),
                };
                if modifier == 1 {
                    format!("\x1b[{code}~").into_bytes()
                } else {
                    format!("\x1b[{code};{modifier}~").into_bytes()
                }
            }
            _ => Vec::new(),
        },
        KeyBase::Named(
            NamedKey::Home
            | NamedKey::End
            | NamedKey::PageUp
            | NamedKey::PageDown
            | NamedKey::Insert
            | NamedKey::Delete,
        ) => unreachable!("handled by editing_sequence"),
        KeyBase::Char(c) => {
            let mut bytes = Vec::new();
            if chord.alt {
                bytes.push(b'\x1b');
            }
            if chord.ctrl {
                let lower = c.to_ascii_lowercase();
                let ctrl_byte = match lower {
                    'a'..='z' => (lower as u8) - b'a' + 1,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '/' | '7' => 0x1f,
                    '?' | '8' => 0x7f,
                    '@' | '2' | ' ' => 0x00,
                    _ => lower as u8,
                };
                bytes.push(ctrl_byte);
            } else {
                let ch = if chord.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            bytes
        }
    }
}

fn is_simple_shell_token(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':' | b'=')
        })
}

fn format_shell_token(token: &str) -> String {
    if is_simple_shell_token(token) {
        token.to_owned()
    } else {
        shell_word(token)
    }
}

fn build_agent_command_line(
    env: &BTreeMap<String, String>,
    command: &str,
    args: &[String],
) -> String {
    let mut parts = Vec::with_capacity(env.len() + 1 + args.len());
    for (key, value) in env {
        if is_simple_shell_token(key) && is_simple_shell_token(value) {
            parts.push(format!("{key}={value}"));
        } else {
            parts.push(format!("{key}={}", shell_word(value)));
        }
    }
    parts.push(format_shell_token(command));
    for arg in args {
        parts.push(format_shell_token(arg));
    }
    parts.join(" ")
}

impl TerminalBackend for MuqunTermBackend {
    fn metadata(&self) -> BackendFuture<'_, BackendMetadata> {
        Box::pin(async move {
            let _ = self.list_alive_sessions().await?;
            Ok(BackendMetadata {
                kind: BackendKind::Herdr,
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
                protocol: None,
                compatibility_response: None,
            })
        })
    }

    fn activity_stream(&self) -> BackendFuture<'_, BackendActivityStream> {
        let backend = self.clone();
        Box::pin(async move {
            let initial = backend.topology_fingerprint().await?;
            let activity = async_stream::stream! {
                let mut delay = ACTIVITY_POLL_MIN;
                let mut previous = Some(initial);
                loop {
                    tokio::time::sleep(delay).await;
                    match backend.topology_fingerprint().await {
                        Ok(fingerprint) => {
                            let changed = previous.as_ref().is_some_and(|last| last != &fingerprint);
                            previous = Some(fingerprint);
                            delay = if changed {
                                ACTIVITY_POLL_MIN
                            } else {
                                (delay * 2).min(ACTIVITY_POLL_MAX)
                            };
                            if changed {
                                yield Ok(BackendActivity {
                                    name: "layout_updated".into(),
                                    payload: json!({
                                        "event": "layout_updated",
                                        "data": { "backend": "muqun_term" }
                                    }),
                                });
                            }
                        }
                        Err(failure) => {
                            yield Err(failure);
                            break;
                        }
                    }
                }
            };
            Ok(Box::pin(activity) as BackendActivityStream)
        })
    }

    fn list_workspaces(&self) -> BackendFuture<'_, Vec<Workspace>> {
        Box::pin(async move {
            let sessions = self.list_alive_sessions().await?;
            let count = u32::try_from(sessions.len()).unwrap_or(u32::MAX);
            let active_tab_id = sessions.first().map(|s| TabId::new(&s.id));
            let agent_status = sessions
                .iter()
                .map(|s| AgentStatus::parse(s.agent_status.as_deref()))
                .find(|status| *status != AgentStatus::Unknown)
                .unwrap_or(AgentStatus::Unknown);
            Ok(vec![Workspace {
                id: WorkspaceId::new(MUQUN_WORKSPACE_ID),
                number: Some(1),
                label: MUQUN_WORKSPACE_LABEL.to_owned(),
                focused: true,
                active_tab_id,
                tab_count: Some(count),
                pane_count: Some(count),
                agent_status,
                repo_root: None,
                checkout_path: None,
            }])
        })
    }

    fn list_tabs(&self) -> BackendFuture<'_, Vec<Tab>> {
        Box::pin(async move {
            let sessions = self.list_alive_sessions().await?;
            Ok(sessions
                .into_iter()
                .enumerate()
                .map(|(index, s)| Tab {
                    id: TabId::new(&s.id),
                    workspace_id: WorkspaceId::new(MUQUN_WORKSPACE_ID),
                    label: s.label.clone().unwrap_or_else(|| s.id.clone()),
                    focused: index == 0,
                    active_pane_id: Some(PaneId::new(&s.id)),
                    pane_count: Some(1),
                })
                .collect())
        })
    }

    fn list_panes(&self) -> BackendFuture<'_, Vec<Pane>> {
        Box::pin(async move {
            let sessions = self.list_alive_sessions().await?;
            Ok(sessions
                .iter()
                .enumerate()
                .map(|(index, s)| pane_from_session(s, index == 0))
                .collect())
        })
    }

    fn list_agents(&self) -> BackendFuture<'_, Vec<Agent>> {
        Box::pin(async move {
            let panes = self.list_panes().await?;
            Ok(panes
                .into_iter()
                .filter(|pane| pane.agent.is_some() || pane.agent_status != AgentStatus::Unknown)
                .map(agent_from_pane)
                .collect())
        })
    }

    fn get_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
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
            Ok(agent_from_pane(pane))
        })
    }

    fn read_pane<'a>(&'a self, request: &'a ReadPane) -> BackendFuture<'a, PaneOutput> {
        Box::pin(async move {
            let read_res = self
                .request(json!({
                    "cmd": "read",
                    "id": request.pane_id.as_str(),
                    "lines": request.lines,
                }))
                .await;

            match read_res {
                Ok(response) => {
                    let raw = response
                        .get("scrollback")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let text = if request.format == OutputFormat::Text {
                        strip_ansi_codes(raw)
                    } else {
                        raw.to_owned()
                    };
                    Ok(PaneOutput {
                        text,
                        revision: None,
                        range: None,
                    })
                }
                Err(BackendError::Unavailable | BackendError::InvalidTarget(_)) => {
                    Err(read_res.unwrap_err())
                }
                Err(_) => {
                    let response = self
                        .request(json!({
                            "cmd": "attach",
                            "id": request.pane_id.as_str(),
                            "raw": false,
                        }))
                        .await?;
                    let encoded = response
                        .get("scrollback")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let decoded_bytes = base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap_or_else(|_| encoded.as_bytes().to_vec());
                    let raw = String::from_utf8_lossy(&decoded_bytes);
                    let formatted = if request.format == OutputFormat::Text {
                        strip_ansi_codes(&raw)
                    } else {
                        raw.into_owned()
                    };
                    let text = tail_lines(&formatted, request.lines);
                    Ok(PaneOutput {
                        text,
                        revision: None,
                        range: None,
                    })
                }
            }
        })
    }

    fn create_workspace<'a>(
        &'a self,
        request: &'a CreateWorkspace,
    ) -> BackendFuture<'a, Workspace> {
        Box::pin(async move {
            let created_id = self
                .create_session(request.cwd.as_deref(), request.label.as_deref())
                .await?;
            Ok(Workspace {
                id: WorkspaceId::new(MUQUN_WORKSPACE_ID),
                number: Some(1),
                label: request
                    .label
                    .clone()
                    .unwrap_or_else(|| MUQUN_WORKSPACE_LABEL.to_owned()),
                focused: true,
                active_tab_id: Some(TabId::new(created_id)),
                tab_count: Some(1),
                pane_count: Some(1),
                agent_status: AgentStatus::Unknown,
                repo_root: None,
                checkout_path: None,
            })
        })
    }

    fn focus_workspace<'a>(&'a self, _id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn rename_workspace<'a>(
        &'a self,
        _id: &'a WorkspaceId,
        _label: &'a str,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn close_workspace<'a>(&'a self, id: &'a WorkspaceId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            if id.as_str() == MUQUN_WORKSPACE_ID {
                let sessions = self.list_alive_sessions().await?;
                for session in sessions {
                    let _ = self
                        .request(json!({ "cmd": "close", "id": session.id }))
                        .await;
                }
                Ok(())
            } else {
                self.request(json!({ "cmd": "close", "id": id.as_str() }))
                    .await?;
                Ok(())
            }
        })
    }

    fn create_tab<'a>(&'a self, request: &'a CreateTab) -> BackendFuture<'a, Tab> {
        Box::pin(async move {
            let created_id = self
                .create_session(request.cwd.as_deref(), request.label.as_deref())
                .await?;
            Ok(Tab {
                id: TabId::new(&created_id),
                workspace_id: request
                    .workspace_id
                    .clone()
                    .unwrap_or_else(|| WorkspaceId::new(MUQUN_WORKSPACE_ID)),
                label: request.label.clone().unwrap_or_else(|| created_id.clone()),
                focused: true,
                active_pane_id: Some(PaneId::new(&created_id)),
                pane_count: Some(1),
            })
        })
    }

    fn focus_tab<'a>(&'a self, _id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn rename_tab<'a>(&'a self, _id: &'a TabId, _label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn close_tab<'a>(&'a self, id: &'a TabId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.request(json!({ "cmd": "close", "id": id.as_str() }))
                .await?;
            Ok(())
        })
    }

    fn focus_pane<'a>(&'a self, _id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn rename_pane<'a>(&'a self, _id: &'a PaneId, _label: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn close_pane<'a>(&'a self, id: &'a PaneId) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            self.request(json!({ "cmd": "close", "id": id.as_str() }))
                .await?;
            Ok(())
        })
    }

    fn split_pane<'a>(&'a self, request: &'a SplitPane) -> BackendFuture<'a, Pane> {
        Box::pin(async move {
            let created_id = self.create_session(request.cwd.as_deref(), None).await?;
            let pane_id = PaneId::new(&created_id);
            if let Ok(pane) = self.get_pane(&pane_id).await {
                return Ok(pane);
            }
            Ok(Pane {
                history_identity: None,
                id: pane_id,
                terminal_id: Some(created_id.clone()),
                workspace_id: WorkspaceId::new(MUQUN_WORKSPACE_ID),
                tab_id: TabId::new(&created_id),
                label: Some(created_id),
                terminal_title: None,
                cwd: request.cwd.clone(),
                focused: true,
                width: Some(u32::from(DEFAULT_COLS)),
                height: Some(u32::from(DEFAULT_ROWS)),
                revision: None,
                foreground_command: None,
                agent: None,
                agent_status: AgentStatus::Unknown,
                max_offset_from_bottom: None,
                viewport_rows: Some(u32::from(DEFAULT_ROWS)),
                alternate_on: None,
                cursor_x: None,
                cursor_y: None,
                shell_pid: None,
            })
        })
    }

    fn send_text<'a>(
        &'a self,
        id: &'a PaneId,
        text: &'a str,
        _mode: SendTextMode,
    ) -> BackendFuture<'a, ()> {
        Box::pin(async move { self.send_input_bytes(id.as_str(), text.as_bytes()).await })
    }

    fn send_keys<'a>(&'a self, id: &'a PaneId, keys: &'a [String]) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let chords = keys
                .iter()
                .map(|key| KeyChord::parse(key).ok_or_else(|| unknown_key(key)))
                .collect::<Result<Vec<_>, _>>()?;
            let mut bytes = Vec::new();
            for chord in &chords {
                bytes.extend_from_slice(&chord_to_bytes(chord));
            }
            self.send_input_bytes(id.as_str(), &bytes).await
        })
    }

    fn keyboard<'a>(
        &'a self,
        _metadata: &'a BackendMetadata,
    ) -> BackendFuture<'a, Option<KeyboardVocabulary>> {
        Box::pin(async move { Ok(Some(KeyboardVocabulary::new(&NamedKey::ALL, true))) })
    }

    fn pane_extended_keys<'a>(&'a self, _id: &'a PaneId) -> BackendFuture<'a, Option<bool>> {
        Box::pin(async { Ok(Some(true)) })
    }

    fn focus_agent<'a>(&'a self, _target: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn prompt_agent<'a>(&'a self, target: &'a str, text: &'a str) -> BackendFuture<'a, ()> {
        Box::pin(async move {
            let payload = format!("{text}\r");
            self.send_input_bytes(target, payload.as_bytes()).await
        })
    }

    fn needs_submit_keypress(&self) -> BackendFuture<'_, bool> {
        Box::pin(async { Ok(false) })
    }

    fn takes_launch_prompt(&self) -> bool {
        true
    }

    fn start_agent<'a>(&'a self, request: &'a StartAgent) -> BackendFuture<'a, StartedAgent> {
        Box::pin(async move {
            let cmd_line = build_agent_command_line(&request.env, &request.command, &request.args);
            let payload = format!("{cmd_line}\n");
            self.send_input_bytes(request.pane_id.as_str(), payload.as_bytes())
                .await?;
            Ok(StartedAgent {
                target: Some(request.pane_id.as_str().to_owned()),
                instance_id: Some(request.pane_id.as_str().to_owned()),
                argv: None,
                env_applied: true,
            })
        })
    }

    fn probe_reachable(&self) -> BackendFuture<'_, bool> {
        Box::pin(async move {
            #[cfg(unix)]
            {
                Ok(UnixStream::connect(&self.socket_path).await.is_ok())
            }
            #[cfg(not(unix))]
            {
                Ok(false)
            }
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::net::UnixListener;

    fn temp_socket_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "muqun-term-test-{label}-{}.sock",
            uuid::Uuid::new_v4()
        ))
    }

    async fn spawn_mock_daemon(
        requests: Arc<Mutex<Vec<Value>>>,
        read_unsupported: bool,
    ) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let socket_path = temp_socket_path("daemon");
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind mock term.sock");
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let requests = Arc::clone(&requests);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(line.trim()).expect("valid json request");
                    requests.lock().unwrap().push(req.clone());
                    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
                    let reply = match cmd {
                        "list" => json!({
                            "ok": true,
                            "sessions": [
                                {
                                    "id": "muqun-term-1",
                                    "cwd": "/workspace/alpha",
                                    "uptime_secs": 42,
                                    "alive": true,
                                    "cols": 120,
                                    "rows": 36,
                                    "label": "Alpha Shell",
                                    "shell_pid": 4321,
                                    "exit_code": null,
                                    "agent_status": "working",
                                    "task_summary": "Running cargo test"
                                },
                                {
                                    "id": "muqun-term-dead",
                                    "cwd": "/workspace/old",
                                    "uptime_secs": 10,
                                    "alive": false,
                                    "cols": 80,
                                    "rows": 24
                                }
                            ]
                        }),
                        "read" => {
                            if read_unsupported {
                                json!({
                                    "ok": false,
                                    "error": "unknown variant `read`"
                                })
                            } else {
                                json!({
                                    "ok": true,
                                    "id": req["id"],
                                    "scrollback": "\x1b[32mhello\x1b[0m\nworld"
                                })
                            }
                        }
                        "attach" => {
                            let encoded = base64::engine::general_purpose::STANDARD
                                .encode(b"\x1b[36mfallback line 1\x1b[0m\nfallback line 2\n");
                            json!({
                                "ok": true,
                                "id": req["id"],
                                "scrollback": encoded
                            })
                        }
                        "create" => json!({
                            "ok": true,
                            "id": "muqun-term-2"
                        }),
                        "input" | "resize" | "close" => json!({
                            "ok": true
                        }),
                        _ => json!({
                            "ok": false,
                            "error": "unsupported command"
                        }),
                    };
                    let mut out = serde_json::to_vec(&reply).unwrap();
                    out.push(b'\n');
                    let _ = write_half.write_all(&out).await;
                    let _ = write_half.flush().await;
                });
            }
        });
        (socket_path, handle)
    }

    #[tokio::test]
    async fn lists_alive_sessions_as_workspace_tabs_and_panes() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = spawn_mock_daemon(Arc::clone(&recorded), false).await;
        let backend = MuqunTermBackend::new(&sock);

        assert!(backend.probe_reachable().await.unwrap());

        let workspaces = backend.list_workspaces().await.unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].id, WorkspaceId::new("muqun-workspace"));
        assert_eq!(workspaces[0].label, "Muqun Terminal");
        assert_eq!(workspaces[0].tab_count, Some(1));
        assert_eq!(workspaces[0].agent_status, AgentStatus::Working);

        let tabs = backend.list_tabs().await.unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, TabId::new("muqun-term-1"));
        assert_eq!(tabs[0].label, "Alpha Shell");

        let panes = backend.list_panes().await.unwrap();
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!(pane.id, PaneId::new("muqun-term-1"));
        assert_eq!(pane.terminal_id.as_deref(), Some("muqun-term-1"));
        assert_eq!(pane.workspace_id, WorkspaceId::new("muqun-workspace"));
        assert_eq!(pane.tab_id, TabId::new("muqun-term-1"));
        assert_eq!(pane.label.as_deref(), Some("Alpha Shell"));
        assert_eq!(pane.cwd.as_deref(), Some(Path::new("/workspace/alpha")));
        assert_eq!(pane.width, Some(120));
        assert_eq!(pane.height, Some(36));
        assert_eq!(pane.shell_pid, Some(4321));
        assert_eq!(pane.agent_status, AgentStatus::Working);

        handle.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn reads_pane_via_read_cmd_and_falls_back_to_attach() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = spawn_mock_daemon(Arc::clone(&recorded), false).await;
        let backend = MuqunTermBackend::new(&sock);

        let output = backend
            .read_pane(&ReadPane {
                pane_id: PaneId::new("muqun-term-1"),
                source: super::super::OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 100,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(output.text, "hello\nworld");

        handle.abort();
        let _ = std::fs::remove_file(&sock);

        // Now test fallback to `attach` when `read` is unsupported
        let recorded_fallback = Arc::new(Mutex::new(Vec::new()));
        let (sock2, handle2) = spawn_mock_daemon(Arc::clone(&recorded_fallback), true).await;
        let backend2 = MuqunTermBackend::new(&sock2);

        let fallback_output = backend2
            .read_pane(&ReadPane {
                pane_id: PaneId::new("muqun-term-1"),
                source: super::super::OutputSource::RecentUnwrapped,
                format: OutputFormat::Text,
                lines: 100,
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(fallback_output.text, "fallback line 1\nfallback line 2");

        handle2.abort();
        let _ = std::fs::remove_file(&sock2);
    }

    #[tokio::test]
    async fn creates_tabs_sends_text_keys_and_starts_agent() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let (sock, handle) = spawn_mock_daemon(Arc::clone(&recorded), false).await;
        let backend = MuqunTermBackend::new(&sock);

        let created_tab = backend
            .create_tab(&CreateTab {
                workspace_id: Some(WorkspaceId::new("muqun-workspace")),
                cwd: Some(PathBuf::from("/workspace/new")),
                label: Some("Worker".into()),
                focus: true,
                env: None,
            })
            .await
            .unwrap();
        assert_eq!(created_tab.id, TabId::new("muqun-term-2"));
        assert_eq!(created_tab.label, "Worker");

        let pane_id = PaneId::new("muqun-term-1");
        backend
            .send_text(&pane_id, "echo hi", SendTextMode::Paste)
            .await
            .unwrap();

        backend
            .send_keys(
                &pane_id,
                &[
                    "Enter".to_owned(),
                    "Up".to_owned(),
                    "ctrl+c".to_owned(),
                    "Home".to_owned(),
                ],
            )
            .await
            .unwrap();

        let mut env = BTreeMap::new();
        env.insert("FOO".to_owned(), "bar".to_owned());
        let started = backend
            .start_agent(&StartAgent {
                pane_id: pane_id.clone(),
                kind: "claude".into(),
                command: "claude".into(),
                executable: None,
                args: vec!["--print".into()],
                timeout_ms: 5000,
                env,
            })
            .await
            .unwrap();
        assert_eq!(started.target.as_deref(), Some("muqun-term-1"));
        assert_eq!(started.instance_id.as_deref(), Some("muqun-term-1"));
        assert!(started.env_applied);

        let log = recorded.lock().unwrap().clone();
        assert_eq!(log[0]["cmd"], "create");
        assert_eq!(log[0]["cwd"], "/workspace/new");
        assert_eq!(log[0]["label"], "Worker");

        assert_eq!(log[1]["cmd"], "input");
        let sent_text = base64::engine::general_purpose::STANDARD
            .decode(log[1]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(sent_text, b"echo hi");

        assert_eq!(log[2]["cmd"], "input");
        let sent_keys = base64::engine::general_purpose::STANDARD
            .decode(log[2]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(sent_keys, b"\r\x1b[A\x03\x1b[1~");

        assert_eq!(log[3]["cmd"], "input");
        let sent_agent = base64::engine::general_purpose::STANDARD
            .decode(log[3]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(sent_agent, b"FOO=bar claude --print\n");

        handle.abort();
        let _ = std::fs::remove_file(&sock);
    }
}
