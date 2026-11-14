//! Persistent terminal daemon for Muqun Desktop.
//!
//! Provides a headless daemon that manages persistent PTY sessions (`DaemonSession`)
//! outliving the graphical UI lifecycle (similar to tmux or herdr-core).
//!
//! Clients communicate over a local Unix domain socket using line-delimited JSON
//! control messages, with optional raw PTY byte streaming on attach.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch};

use crate::platform::pty::{self, Child, Exit, Pty, WinSize};

/// Default socket filename placed in the user configuration directory.
pub const DEFAULT_SOCKET_NAME: &str = "term.sock";

/// Default scrollback buffer capacity per session (1 MiB).
pub const DEFAULT_SCROLLBACK_CAPACITY: usize = 1024 * 1024;

/// Default terminal columns.
pub const DEFAULT_COLS: u16 = 80;

/// Default terminal rows.
pub const DEFAULT_ROWS: u16 = 24;

fn default_cols() -> u16 {
    DEFAULT_COLS
}

fn default_rows() -> u16 {
    DEFAULT_ROWS
}

/// Returns the platform default socket path:
/// `~/.config/muqun-desktop/term.sock` on Linux.
pub fn default_socket_path() -> PathBuf {
    if let Some(config_dir) = dirs::config_dir() {
        config_dir
            .join(crate::platform::DIR_NAME)
            .join(DEFAULT_SOCKET_NAME)
    } else {
        std::env::temp_dir().join(format!(
            "{}-{}",
            crate::platform::DIR_NAME,
            DEFAULT_SOCKET_NAME
        ))
    }
}

// ============================================================================
// Scrollback Ring Buffer
// ============================================================================

/// A bounded byte ring buffer for terminal output history.
///
/// When the buffer reaches `max_bytes`, the oldest bytes are evicted to
/// make room for new output, preserving terminal state continuity on client
/// reattachment.
#[derive(Debug, Clone)]
pub struct ScrollbackBuffer {
    max_bytes: usize,
    buffer: VecDeque<u8>,
}

impl ScrollbackBuffer {
    /// Creates a new buffer with the specified maximum byte capacity.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            buffer: VecDeque::with_capacity(max_bytes.min(64 * 1024)),
        }
    }

    /// Creates a buffer with default capacity (1 MiB).
    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_SCROLLBACK_CAPACITY)
    }

    /// Appends bytes to the buffer, evicting oldest data if capacity is exceeded.
    pub fn push(&mut self, data: &[u8]) {
        if self.max_bytes == 0 || data.is_empty() {
            return;
        }
        if data.len() >= self.max_bytes {
            self.buffer.clear();
            self.buffer.extend(&data[data.len() - self.max_bytes..]);
            return;
        }
        let excess = (self.buffer.len() + data.len()).saturating_sub(self.max_bytes);
        if excess > 0 {
            self.buffer.drain(..excess);
        }
        self.buffer.extend(data);
    }

    /// Returns a contiguous copy of the buffered bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        let (s1, s2) = self.buffer.as_slices();
        let mut out = Vec::with_capacity(s1.len() + s2.len());
        out.extend_from_slice(s1);
        out.extend_from_slice(s2);
        out
    }

    /// Number of bytes currently held in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the buffer contains no data.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Clears the buffer.
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

// ============================================================================
// IPC Wire Protocol Messages
// ============================================================================

/// Client command requests sent across the domain socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Create a new persistent PTY session.
    Create {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default)]
        cell_w: Option<u16>,
        #[serde(default)]
        cell_h: Option<u16>,
        #[serde(default)]
        label: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
    /// Attach to an existing session to stream scrollback and live output.
    Attach {
        id: String,
        #[serde(default)]
        raw: bool,
    },
    /// Write input bytes (base64 or raw string) to the session's PTY master.
    Input { id: String, data: String },
    /// Resize the session's terminal dimensions.
    Resize {
        id: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        cell_w: Option<u16>,
        #[serde(default)]
        cell_h: Option<u16>,
    },
    /// Read recent scrollback text from a session without attaching a stream.
    Read {
        id: String,
        #[serde(default)]
        lines: Option<u32>,
    },
    /// Terminate and close a persistent session.
    Close { id: String },
    /// List all active and recent sessions.
    List,
}

/// Metadata descriptor for a session reported in `list`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub cwd: PathBuf,
    pub uptime_secs: u64,
    pub alive: bool,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_summary: Option<String>,
}

/// Server response frame returned for control requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn success() -> Self {
        Self {
            ok: true,
            id: None,
            sessions: None,
            scrollback: None,
            error: None,
        }
    }

    pub fn id(id: String) -> Self {
        Self {
            ok: true,
            id: Some(id),
            sessions: None,
            scrollback: None,
            error: None,
        }
    }

    pub fn sessions(sessions: Vec<SessionInfo>) -> Self {
        Self {
            ok: true,
            id: None,
            sessions: Some(sessions),
            scrollback: None,
            error: None,
        }
    }

    pub fn attached(id: String, scrollback: String) -> Self {
        Self {
            ok: true,
            id: Some(id),
            sessions: None,
            scrollback: Some(scrollback),
            error: None,
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            id: None,
            sessions: None,
            scrollback: None,
            error: Some(msg.into()),
        }
    }
}

/// Asynchronous events streamed to attached clients in JSON mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum DaemonEvent {
    Output {
        data: String,
    },
    Status {
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_status: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_summary: Option<String>,
    },
    Exit {
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
    },
}

/// Internal broadcast message for subscriber distribution.
#[derive(Debug, Clone)]
pub enum OutputMsg {
    Data(Vec<u8>),
    Status {
        cwd: Option<PathBuf>,
        agent_status: Option<String>,
        exit_code: Option<i32>,
        task_summary: Option<String>,
    },
    Exit(Exit),
}

// ============================================================================
// ANSI Stripping & Output Inspection Helpers
// ============================================================================

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strips ANSI escape sequences (CSI `\x1b[...`, OSC `\x1b]...(\x07|\x1b\\)`,
/// and 2-byte/3-byte ESC sequences) and normalizes `\r\n` to `\n`.
pub fn strip_ansi_codes(bytes: &[u8]) -> String {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1).copied() {
                Some(b'[') => {
                    i += 2;
                    while i < bytes.len() {
                        let b = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&b) {
                            break;
                        }
                    }
                }
                Some(b']') | Some(b'P') | Some(b'X') | Some(b'^') | Some(b'_') => {
                    i += 2;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                Some(b'(') | Some(b')') | Some(b'*') | Some(b'+') | Some(b'-') | Some(b'.')
                | Some(b'/') => {
                    i = (i + 3).min(bytes.len());
                }
                Some(_) => {
                    i += 2;
                }
                None => {
                    i += 1;
                }
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).replace("\r\n", "\n")
}

/// Inspects a raw PTY output chunk for OSC 7 (CWD), OSC 133 (shell integration),
/// and task completion markers (`MUQUN_TASK_DONE:` / `MUQUN_TASK_BLOCKED:`).
///
/// Updates the session state locks in place and returns a `DaemonEvent::Status`
/// if any field was updated.
pub fn inspect_output_chunk(
    chunk: &[u8],
    live_cwd: &Mutex<PathBuf>,
    agent_status: &Mutex<Option<String>>,
    exit_code: &Mutex<Option<i32>>,
    task_summary: &Mutex<Option<String>>,
) -> Option<DaemonEvent> {
    let mut changed_cwd: Option<PathBuf> = None;
    let mut changed_status: Option<String> = None;
    let mut changed_exit_code: Option<i32> = None;
    let mut changed_summary: Option<String> = None;

    // 1. Scan for OSC sequences (\x1b] ... (\x07 | \x1b\\))
    let mut i = 0;
    while i + 1 < chunk.len() {
        if chunk[i] == 0x1b && chunk[i + 1] == b']' {
            let start = i + 2;
            let mut end = None;
            let mut next_i = start;
            let mut j = start;
            while j < chunk.len() {
                if chunk[j] == 0x07 {
                    end = Some(j);
                    next_i = j + 1;
                    break;
                }
                if chunk[j] == 0x1b && chunk.get(j + 1) == Some(&b'\\') {
                    end = Some(j);
                    next_i = j + 2;
                    break;
                }
                j += 1;
            }
            if let Some(end_idx) = end {
                let osc_str = String::from_utf8_lossy(&chunk[start..end_idx]);
                if let Some(rest) = osc_str.strip_prefix("7;file://") {
                    if let Some(slash_idx) = rest.find('/') {
                        let decoded = percent_decode(&rest[slash_idx..]);
                        if decoded.starts_with('/') {
                            let new_path = PathBuf::from(decoded);
                            if let Ok(mut guard) = live_cwd.lock() {
                                *guard = new_path.clone();
                            }
                            changed_cwd = Some(new_path);
                        }
                    }
                } else if let Some(rest) = osc_str.strip_prefix("133;") {
                    if rest == "A" || rest.starts_with("A;") {
                        if let Ok(mut guard) = agent_status.lock() {
                            let keep = matches!(guard.as_deref(), Some("done") | Some("blocked"));
                            if !keep {
                                *guard = Some("idle".to_string());
                                changed_status = Some("idle".to_string());
                            }
                        }
                    } else if rest == "C" || rest.starts_with("C;") {
                        if let Ok(mut guard) = agent_status.lock() {
                            *guard = Some("working".to_string());
                            changed_status = Some("working".to_string());
                        }
                    } else if rest == "D" || rest.starts_with("D;") {
                        if let Some(after_d) = rest.strip_prefix("D;") {
                            let code_part = after_d.split(';').next().unwrap_or("");
                            if let Ok(code) = code_part.trim().parse::<i32>() {
                                if let Ok(mut guard) = exit_code.lock() {
                                    *guard = Some(code);
                                }
                                changed_exit_code = Some(code);
                            }
                        }
                        if let Ok(mut guard) = agent_status.lock() {
                            let keep = matches!(guard.as_deref(), Some("done") | Some("blocked"));
                            if !keep {
                                *guard = Some("idle".to_string());
                                changed_status = Some("idle".to_string());
                            }
                        }
                    }
                }
                i = next_i;
                continue;
            }
        }
        i += 1;
    }

    // 2. Scan lines for task completion markers
    let text = String::from_utf8_lossy(chunk);
    for line in text.lines() {
        if let Some(idx) = line.find("MUQUN_TASK_DONE:") {
            let after = &line[idx + "MUQUN_TASK_DONE:".len()..];
            let summary = strip_ansi_codes(after.as_bytes()).trim().to_string();
            if !summary.is_empty() && !summary.contains("<one-line summary>") {
                if let Ok(mut guard) = agent_status.lock() {
                    *guard = Some("done".to_string());
                }
                if let Ok(mut guard) = task_summary.lock() {
                    *guard = Some(summary.clone());
                }
                changed_status = Some("done".to_string());
                changed_summary = Some(summary);
            }
        } else if let Some(idx) = line.find("MUQUN_TASK_BLOCKED:") {
            let after = &line[idx + "MUQUN_TASK_BLOCKED:".len()..];
            let reason = strip_ansi_codes(after.as_bytes()).trim().to_string();
            if !reason.is_empty() && !reason.contains("<one-line reason>") {
                if let Ok(mut guard) = agent_status.lock() {
                    *guard = Some("blocked".to_string());
                }
                if let Ok(mut guard) = task_summary.lock() {
                    *guard = Some(reason.clone());
                }
                changed_status = Some("blocked".to_string());
                changed_summary = Some(reason);
            }
        }
    }

    if changed_cwd.is_some()
        || changed_status.is_some()
        || changed_exit_code.is_some()
        || changed_summary.is_some()
    {
        Some(DaemonEvent::Status {
            cwd: changed_cwd,
            agent_status: changed_status,
            exit_code: changed_exit_code,
            task_summary: changed_summary,
        })
    } else {
        None
    }
}

// ============================================================================
// DaemonSession
// ============================================================================

/// A persistent PTY session that runs detached in the background.
pub struct DaemonSession {
    pub id: String,
    pub cwd: PathBuf,
    pub created_at: Instant,
    live_cwd: Arc<Mutex<PathBuf>>,
    label: Mutex<Option<String>>,
    shell_pid: Option<u32>,
    exit_code: Arc<Mutex<Option<i32>>>,
    agent_status: Arc<Mutex<Option<String>>>,
    task_summary: Arc<Mutex<Option<String>>>,
    size: Mutex<WinSize>,
    pty: Mutex<Option<Pty>>,
    child: Mutex<Option<Child>>,
    scrollback: Arc<Mutex<ScrollbackBuffer>>,
    broadcast_tx: broadcast::Sender<OutputMsg>,
    alive: Arc<AtomicBool>,
    exit_status: Arc<Mutex<Option<Exit>>>,
}

impl DaemonSession {
    /// Spawns a new shell process in a PTY session.
    pub fn spawn(
        id: String,
        cwd: PathBuf,
        size: WinSize,
        scrollback_capacity: usize,
    ) -> io::Result<Arc<Self>> {
        let alive = Arc::new(AtomicBool::new(true));
        let exit_status = Arc::new(Mutex::new(None));
        let exit_code = Arc::new(Mutex::new(None));
        let live_cwd = Arc::new(Mutex::new(cwd.clone()));
        let agent_status = Arc::new(Mutex::new(Some("idle".to_string())));
        let task_summary = Arc::new(Mutex::new(None));
        let (broadcast_tx, _rx) = broadcast::channel(1024);

        let alive_flag = alive.clone();
        let exit_slot = exit_status.clone();
        let exit_code_slot = exit_code.clone();
        let exit_broadcast = broadcast_tx.clone();

        let on_exit: pty::OnExit = Box::new(move |exit| {
            alive_flag.store(false, Ordering::SeqCst);
            if let Ok(mut slot) = exit_slot.lock() {
                *slot = Some(exit);
            }
            if let Exit::Code(c) = exit {
                if let Ok(mut code_guard) = exit_code_slot.lock() {
                    *code_guard = Some(c);
                }
            }
            let _ = exit_broadcast.send(OutputMsg::Exit(exit));
        });

        let (pty, child) = pty::spawn_shell(&cwd, size, on_exit)?;
        let reader = pty.reader()?;

        let scrollback = Arc::new(Mutex::new(ScrollbackBuffer::new(scrollback_capacity)));
        let scrollback_writer = scrollback.clone();
        let stream_tx = broadcast_tx.clone();
        let alive_for_reader = alive.clone();
        let reader_cwd = live_cwd.clone();
        let reader_status = agent_status.clone();
        let reader_exit_code = exit_code.clone();
        let reader_summary = task_summary.clone();

        let reader_thread_name = format!("term-reader-{}", id);
        std::thread::Builder::new()
            .name(reader_thread_name)
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let chunk = &buf[..n];
                            if let Ok(mut sb) = scrollback_writer.lock() {
                                sb.push(chunk);
                            }
                            let _ = stream_tx.send(OutputMsg::Data(chunk.to_vec()));
                            if let Some(DaemonEvent::Status {
                                cwd,
                                agent_status,
                                exit_code,
                                task_summary,
                            }) = inspect_output_chunk(
                                chunk,
                                &reader_cwd,
                                &reader_status,
                                &reader_exit_code,
                                &reader_summary,
                            ) {
                                let _ = stream_tx.send(OutputMsg::Status {
                                    cwd,
                                    agent_status,
                                    exit_code,
                                    task_summary,
                                });
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                alive_for_reader.store(false, Ordering::SeqCst);
            })?;

        let session = Arc::new(Self {
            id,
            cwd,
            created_at: Instant::now(),
            live_cwd,
            label: Mutex::new(None),
            shell_pid: Some(child.pid() as u32),
            exit_code,
            agent_status,
            task_summary,
            size: Mutex::new(size),
            pty: Mutex::new(Some(pty)),
            child: Mutex::new(Some(child)),
            scrollback,
            broadcast_tx,
            alive,
            exit_status,
        });

        Ok(session)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn live_cwd(&self) -> PathBuf {
        self.live_cwd
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| self.cwd.clone())
    }

    pub fn label(&self) -> Option<String> {
        self.label.lock().ok().and_then(|g| g.clone())
    }

    pub fn set_label(&self, label: Option<String>) {
        if let Ok(mut g) = self.label.lock() {
            *g = label;
        }
    }

    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|g| *g)
    }

    pub fn agent_status(&self) -> Option<String> {
        self.agent_status.lock().ok().and_then(|g| g.clone())
    }

    pub fn set_agent_status(&self, status: Option<String>) {
        if let Ok(mut g) = self.agent_status.lock() {
            *g = status;
        }
    }

    pub fn task_summary(&self) -> Option<String> {
        self.task_summary.lock().ok().and_then(|g| g.clone())
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn exit_status(&self) -> Option<Exit> {
        self.exit_status.lock().ok().and_then(|guard| *guard)
    }

    pub fn uptime(&self) -> Duration {
        self.created_at.elapsed()
    }

    pub fn size(&self) -> WinSize {
        self.size.lock().map(|s| *s).unwrap_or(WinSize {
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
            px_w: 0,
            px_h: 0,
        })
    }

    pub fn scrollback(&self) -> Vec<u8> {
        self.scrollback
            .lock()
            .map(|sb| sb.to_vec())
            .unwrap_or_default()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<OutputMsg> {
        self.broadcast_tx.subscribe()
    }

    /// Writes raw input bytes to the PTY master.
    pub fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let mut guard = self
            .pty
            .lock()
            .map_err(|_| io::Error::other("pty lock poisoned"))?;
        if let Some(pty) = guard.as_mut() {
            pty.write_all(data)
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "pty session is closed",
            ))
        }
    }

    /// Resizes the PTY terminal window dimensions.
    pub fn resize(&self, size: WinSize) {
        if let Ok(mut cur) = self.size.lock() {
            *cur = size;
        }
        if let Ok(guard) = self.pty.lock() {
            if let Some(pty) = guard.as_ref() {
                pty.resize(size);
            }
        }
    }

    /// Closes the PTY session and terminates child processes.
    pub fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Ok(mut pty_guard) = self.pty.lock() {
            pty_guard.take();
        }
        if let Ok(mut child_guard) = self.child.lock() {
            child_guard.take();
        }
    }
}

impl Drop for DaemonSession {
    fn drop(&mut self) {
        self.close();
    }
}

// ============================================================================
// DaemonServer
// ============================================================================

/// Persistent terminal daemon server state.
pub struct DaemonServer {
    sessions: Arc<Mutex<HashMap<String, Arc<DaemonSession>>>>,
    counter: Arc<AtomicU64>,
}

impl Default for DaemonServer {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonServer {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Dispatches a non-streaming request and produces a response.
    pub fn dispatch_request(&self, req: Request) -> Response {
        match req {
            Request::Create {
                id,
                cwd,
                cols,
                rows,
                cell_w,
                cell_h,
                label,
                command,
            } => self.handle_create(id, cwd, cols, rows, cell_w, cell_h, label, command),
            Request::List => self.handle_list(),
            Request::Input { id, data } => self.handle_input(&id, &data),
            Request::Resize {
                id,
                cols,
                rows,
                cell_w,
                cell_h,
            } => self.handle_resize(&id, cols, rows, cell_w, cell_h),
            Request::Read { id, lines } => self.handle_read(&id, lines),
            Request::Close { id } => self.handle_close(&id),
            Request::Attach { .. } => {
                Response::error("attach command cannot be dispatched synchronously")
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_create(
        &self,
        id: Option<String>,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
        cell_w: Option<u16>,
        cell_h: Option<u16>,
        label: Option<String>,
        command: Option<String>,
    ) -> Response {
        let cols = if cols == 0 { DEFAULT_COLS } else { cols };
        let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
        let size = WinSize {
            cols,
            rows,
            px_w: cell_w.unwrap_or(0),
            px_h: cell_h.unwrap_or(0),
        };

        let session_id = match id {
            Some(custom_id) if !custom_id.trim().is_empty() => custom_id.trim().to_string(),
            _ => format!("muqun-term-{}", self.counter.fetch_add(1, Ordering::SeqCst)),
        };

        let working_dir = match cwd {
            Some(dir) if dir.is_dir() => dir,
            _ => dirs::home_dir()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("/")),
        };

        {
            let mut sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return Response::error("daemon sessions lock poisoned"),
            };

            if let Some(existing) = sessions.get(&session_id) {
                if existing.is_alive() {
                    return Response::error(format!(
                        "session id '{}' is already in use",
                        session_id
                    ));
                }
            }

            match DaemonSession::spawn(
                session_id.clone(),
                working_dir,
                size,
                DEFAULT_SCROLLBACK_CAPACITY,
            ) {
                Ok(session) => {
                    if label.is_some() {
                        session.set_label(label);
                    }
                    if let Some(ref cmd) = command {
                        if !cmd.is_empty() {
                            session.set_agent_status(Some("working".to_string()));
                            let line = format!("{}\n", cmd.trim_end_matches('\n'));
                            let _ = session.write_all(line.as_bytes());
                        }
                    }
                    sessions.insert(session_id.clone(), session);
                    Response::id(session_id)
                }
                Err(e) => Response::error(format!("failed to spawn pty shell: {e}")),
            }
        }
    }

    fn handle_list(&self) -> Response {
        let mut sessions = match self.sessions.lock() {
            Ok(s) => s,
            Err(_) => return Response::error("daemon sessions lock poisoned"),
        };

        sessions.retain(|_, s| s.is_alive());

        let mut list = Vec::new();
        for (id, s) in sessions.iter() {
            let size = s.size();
            list.push(SessionInfo {
                id: id.clone(),
                cwd: s.live_cwd(),
                uptime_secs: s.uptime().as_secs(),
                alive: s.is_alive(),
                cols: size.cols,
                rows: size.rows,
                label: s.label(),
                shell_pid: s.shell_pid(),
                exit_code: s.exit_code(),
                agent_status: s.agent_status(),
                task_summary: s.task_summary(),
            });
        }
        list.sort_by(|a, b| a.id.cmp(&b.id));
        Response::sessions(list)
    }

    fn handle_read(&self, id: &str, lines: Option<u32>) -> Response {
        let session = {
            let sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return Response::error("daemon sessions lock poisoned"),
            };
            sessions.get(id).cloned()
        };

        let session = match session {
            Some(s) => s,
            None => return Response::error(format!("session '{id}' not found")),
        };

        let raw_bytes = session.scrollback();
        let stripped = strip_ansi_codes(&raw_bytes);
        let max_lines = lines.unwrap_or(200) as usize;
        let all_lines: Vec<&str> = stripped.lines().collect();
        let start = all_lines.len().saturating_sub(max_lines);
        let text = all_lines[start..].join("\n");

        Response {
            ok: true,
            id: Some(id.to_string()),
            sessions: None,
            scrollback: Some(text),
            error: None,
        }
    }

    fn handle_input(&self, id: &str, data: &str) -> Response {
        let session = {
            let sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return Response::error("daemon sessions lock poisoned"),
            };
            sessions.get(id).cloned()
        };

        let session = match session {
            Some(s) => s,
            None => return Response::error(format!("session '{id}' not found")),
        };

        let bytes = match base64::engine::general_purpose::STANDARD.decode(data.trim()) {
            Ok(b) => b,
            Err(_) => data.as_bytes().to_vec(),
        };

        match session.write_all(&bytes) {
            Ok(()) => Response::success(),
            Err(e) => Response::error(format!("failed to write to pty: {e}")),
        }
    }

    fn handle_resize(
        &self,
        id: &str,
        cols: u16,
        rows: u16,
        cell_w: Option<u16>,
        cell_h: Option<u16>,
    ) -> Response {
        let session = {
            let sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return Response::error("daemon sessions lock poisoned"),
            };
            sessions.get(id).cloned()
        };

        let session = match session {
            Some(s) => s,
            None => return Response::error(format!("session '{id}' not found")),
        };

        let cols = if cols == 0 { DEFAULT_COLS } else { cols };
        let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
        let size = WinSize {
            cols,
            rows,
            px_w: cell_w.unwrap_or(0),
            px_h: cell_h.unwrap_or(0),
        };

        session.resize(size);
        Response::success()
    }

    fn handle_close(&self, id: &str) -> Response {
        let removed = {
            let mut sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return Response::error("daemon sessions lock poisoned"),
            };
            sessions.remove(id)
        };

        if let Some(session) = removed {
            session.close();
            Response::success()
        } else {
            Response::error(format!("session '{id}' not found"))
        }
    }

    /// Handles a new client connection on the Unix domain socket.
    pub async fn handle_connection(
        self: Arc<Self>,
        stream: tokio::net::UnixStream,
    ) -> io::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let req: Request = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(e) => {
                    let resp = Response::error(format!("invalid json request: {e}"));
                    let mut out = serde_json::to_vec(&resp)?;
                    out.push(b'\n');
                    writer.write_all(&out).await?;
                    writer.flush().await?;
                    continue;
                }
            };

            match req {
                Request::Attach { id, raw } => {
                    return self.handle_attach_connection(id, raw, lines, writer).await;
                }
                other => {
                    let resp = self.dispatch_request(other);
                    let mut out = serde_json::to_vec(&resp)?;
                    out.push(b'\n');
                    writer.write_all(&out).await?;
                    writer.flush().await?;
                }
            }
        }

        Ok(())
    }

    async fn handle_attach_connection(
        self: Arc<Self>,
        id: String,
        raw: bool,
        lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        mut writer: tokio::net::unix::OwnedWriteHalf,
    ) -> io::Result<()> {
        let session_res = {
            match self.sessions.lock() {
                Ok(s) => Ok(s.get(&id).cloned()),
                Err(_) => Err("daemon sessions lock poisoned"),
            }
        };

        let session = match session_res {
            Ok(Some(s)) => s,
            Ok(None) => {
                let resp = Response::error(format!("session '{id}' not found"));
                let mut out = serde_json::to_vec(&resp)?;
                out.push(b'\n');
                writer.write_all(&out).await?;
                return Ok(());
            }
            Err(err) => {
                let resp = Response::error(err);
                let mut out = serde_json::to_vec(&resp)?;
                out.push(b'\n');
                writer.write_all(&out).await?;
                return Ok(());
            }
        };

        let mut broadcast_rx = session.subscribe();
        let scrollback_bytes = session.scrollback();

        if raw {
            let resp = Response::success();
            let mut out = serde_json::to_vec(&resp)?;
            out.push(b'\n');
            writer.write_all(&out).await?;
            writer.flush().await?;

            let mut raw_reader = lines.into_inner();
            // Keep polling input while output is awaiting socket capacity. A
            // single select loop with write_all inside its output arm stalls
            // every key (including Ctrl+C) behind a slow renderer.
            let input = async {
                let mut bytes = [0u8; 4096];
                loop {
                    let count = raw_reader.read(&mut bytes).await?;
                    if count == 0 {
                        return Ok::<(), io::Error>(());
                    }
                    session.write_all(&bytes[..count])?;
                }
            };
            let output = async {
                if !scrollback_bytes.is_empty() {
                    writer.write_all(&scrollback_bytes).await?;
                }
                loop {
                    match broadcast_rx.recv().await {
                        Ok(OutputMsg::Data(bytes)) => writer.write_all(&bytes).await?,
                        Ok(OutputMsg::Status { .. }) => continue,
                        Ok(OutputMsg::Exit(_)) | Err(broadcast::error::RecvError::Closed) => {
                            return Ok::<(), io::Error>(());
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    }
                }
            };
            // Both futures belong to the connection; ending either cancels
            // the other without leaving a detached reader or writer behind.
            tokio::select! {
                result = input => result?,
                result = output => result?,
            }
        } else {
            let scrollback_b64 =
                base64::engine::general_purpose::STANDARD.encode(&scrollback_bytes);
            let resp = Response::attached(id.clone(), scrollback_b64);
            let mut out = serde_json::to_vec(&resp)?;
            out.push(b'\n');
            writer.write_all(&out).await?;
            writer.flush().await?;

            let mut lines = lines;
            let this = self.clone();

            loop {
                tokio::select! {
                    line_res = lines.next_line() => {
                        match line_res {
                            Ok(Some(line)) => {
                                let line = line.trim();
                                if line.is_empty() {
                                    continue;
                                }
                                if let Ok(req) = serde_json::from_str::<Request>(line) {
                                    let resp = this.dispatch_request(req);
                                    let mut out = serde_json::to_vec(&resp)?;
                                    out.push(b'\n');
                                    if writer.write_all(&out).await.is_err() {
                                        break;
                                    }
                                    let _ = writer.flush().await;
                                }
                            }
                            _ => break,
                        }
                    }
                    msg_res = broadcast_rx.recv() => {
                        match msg_res {
                            Ok(OutputMsg::Data(bytes)) => {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                                let event = DaemonEvent::Output { data: b64 };
                                let mut out = serde_json::to_vec(&event)?;
                                out.push(b'\n');
                                if writer.write_all(&out).await.is_err() {
                                    break;
                                }
                                let _ = writer.flush().await;
                            }
                            Ok(OutputMsg::Status {
                                cwd,
                                agent_status,
                                exit_code,
                                task_summary,
                            }) => {
                                let event = DaemonEvent::Status {
                                    cwd,
                                    agent_status,
                                    exit_code,
                                    task_summary,
                                };
                                let mut out = serde_json::to_vec(&event)?;
                                out.push(b'\n');
                                if writer.write_all(&out).await.is_err() {
                                    break;
                                }
                                let _ = writer.flush().await;
                            }
                            Ok(OutputMsg::Exit(exit)) => {
                                let (code, signal) = match exit {
                                    Exit::Code(c) => (Some(c), None),
                                    Exit::Signal(s) => (None, Some(s)),
                                };
                                let event = DaemonEvent::Exit { code, signal };
                                let mut out = serde_json::to_vec(&event)?;
                                out.push(b'\n');
                                let _ = writer.write_all(&out).await;
                                let _ = writer.flush().await;
                                break;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// Daemon Runner Functions
// ============================================================================

fn is_socket_alive(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs the persistent terminal daemon server on `socket_path` (or platform default).
pub fn run_daemon(socket_path: Option<PathBuf>) -> io::Result<()> {
    let path = socket_path.unwrap_or_else(default_socket_path);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.block_on(run_daemon_async(path))
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(run_daemon_async(path))
    }
}

/// Asynchronous runner for the persistent terminal daemon.
pub async fn run_daemon_async(path: PathBuf) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if is_socket_alive(&path) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("terminal daemon is already running at {}", path.display()),
        ));
    }

    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let listener = tokio::net::UnixListener::bind(&path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    let server = Arc::new(DaemonServer::new());
    let _cleanup = SocketCleanup(path.clone());

    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let server = server.clone();
                        tokio::spawn(async move {
                            if let Err(e) = server.handle_connection(stream).await {
                                tracing::debug!("terminal daemon connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("terminal daemon accept error: {e}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("terminal daemon received shutdown signal");
                break;
            }
        }
    }

    Ok(())
}

/// Runs the daemon server with an explicit shutdown receiver for tests and embedding.
pub async fn run_server_with_shutdown(
    server: Arc<DaemonServer>,
    listener: tokio::net::UnixListener,
    mut shutdown_rx: watch::Receiver<bool>,
) -> io::Result<()> {
    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((stream, _addr)) => {
                        let s = server.clone();
                        tokio::spawn(async move {
                            let _ = s.handle_connection(stream).await;
                        });
                    }
                    Err(e) => {
                        tracing::debug!("accept error: {e}");
                    }
                }
            }
            res = shutdown_rx.changed() => {
                if res.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Ensures the daemon is running at the default socket path.
///
/// If not running, spawns `current_exe() --term-daemon` detached and waits up to
/// 2 seconds for the socket to accept connections.
pub fn ensure_daemon_running() -> io::Result<PathBuf> {
    ensure_daemon_running_at(None)
}

/// Ensures the daemon is running at `socket_path` (or default if None).
pub fn ensure_daemon_running_at(socket_path: Option<PathBuf>) -> io::Result<PathBuf> {
    let path = socket_path.unwrap_or_else(default_socket_path);
    if is_socket_alive(&path) {
        return Ok(path);
    }

    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--term-daemon");
    cmd.arg(&path);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let _child = cmd.spawn()?;

    let start = Instant::now();
    let timeout = Duration::from_millis(2000);
    while start.elapsed() < timeout {
        if is_socket_alive(&path) {
            return Ok(path);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for terminal daemon socket at {} to become ready",
            path.display()
        ),
    ))
}

// ============================================================================
// DaemonClient
// ============================================================================

/// Client interface to interact with the persistent terminal daemon.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl Default for DaemonClient {
    fn default() -> Self {
        Self::new(None)
    }
}

impl DaemonClient {
    /// Creates a client targeting `socket_path` (or default if None).
    pub fn new(socket_path: Option<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.unwrap_or_else(default_socket_path),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    async fn connect(&self) -> io::Result<tokio::net::UnixStream> {
        tokio::net::UnixStream::connect(&self.socket_path).await
    }

    async fn send_request(&self, req: &Request) -> io::Result<Response> {
        let mut stream = self.connect().await?;
        let mut payload = serde_json::to_vec(req)?;
        payload.push(b'\n');
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let (reader, _) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let first_line = lines.next_line().await?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed connection")
        })?;

        let resp: Response = serde_json::from_str(&first_line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if !resp.ok {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "daemon error".into()),
            ))
        } else {
            Ok(resp)
        }
    }

    /// Creates a persistent session on the daemon.
    pub async fn create_session(
        &self,
        id: Option<String>,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> io::Result<String> {
        self.create_session_with_options(id, cwd, cols, rows, None, None)
            .await
    }

    /// Creates a persistent session on the daemon with optional label and initial command.
    pub async fn create_session_with_options(
        &self,
        id: Option<String>,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
        label: Option<String>,
        command: Option<String>,
    ) -> io::Result<String> {
        let req = Request::Create {
            id,
            cwd,
            cols,
            rows,
            cell_w: None,
            cell_h: None,
            label,
            command,
        };
        let resp = self.send_request(&req).await?;
        resp.id.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing session id in response")
        })
    }

    /// Reads recent plain-text scrollback lines from a session without attaching a stream.
    pub async fn read_session_text(&self, id: &str, lines: Option<u32>) -> io::Result<String> {
        let req = Request::Read {
            id: id.to_string(),
            lines,
        };
        let resp = self.send_request(&req).await?;
        Ok(resp.scrollback.unwrap_or_default())
    }

    /// Lists active and recent sessions on the daemon.
    pub async fn list_sessions(&self) -> io::Result<Vec<SessionInfo>> {
        let req = Request::List;
        let resp = self.send_request(&req).await?;
        Ok(resp.sessions.unwrap_or_default())
    }

    /// Sends input data to a session's PTY stdin.
    pub async fn send_input(&self, id: &str, data: &[u8]) -> io::Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let req = Request::Input {
            id: id.to_string(),
            data: b64,
        };
        self.send_request(&req).await?;
        Ok(())
    }

    /// Resizes a session's terminal dimensions.
    pub async fn resize_session(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        let req = Request::Resize {
            id: id.to_string(),
            cols,
            rows,
            cell_w: None,
            cell_h: None,
        };
        self.send_request(&req).await?;
        Ok(())
    }

    /// Closes a persistent session and reaps its child processes.
    pub async fn close_session(&self, id: &str) -> io::Result<()> {
        let req = Request::Close { id: id.to_string() };
        self.send_request(&req).await?;
        Ok(())
    }

    /// Attaches to a session, returning initial scrollback history and a live output receiver.
    pub async fn attach_session(&self, id: &str) -> io::Result<AttachedSession> {
        let mut stream = self.connect().await?;
        let req = Request::Attach {
            id: id.to_string(),
            raw: false,
        };
        let mut payload = serde_json::to_vec(&req)?;
        payload.push(b'\n');
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let (reader, writer) = stream.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let mut lines = BufReader::new(reader).lines();
        let first_line = lines.next_line().await?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "daemon closed connection")
        })?;

        let resp: Response = serde_json::from_str(&first_line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if !resp.ok {
            return Err(io::Error::other(
                resp.error.unwrap_or_else(|| "attach failed".into()),
            ));
        }

        let scrollback = resp
            .scrollback
            .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
            .unwrap_or_default();

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<DaemonEvent>(line) {
                    match event {
                        DaemonEvent::Output { data } => {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(data)
                            {
                                if tx.send(bytes).await.is_err() {
                                    break;
                                }
                            }
                        }
                        DaemonEvent::Status { .. } => {}
                        DaemonEvent::Exit { .. } => break,
                    }
                }
            }
        });

        Ok(AttachedSession {
            id: id.to_string(),
            scrollback,
            output_rx: rx,
            writer,
            client: self.clone(),
        })
    }

    /// Attaches to a session in raw streaming mode, yielding the connected `UnixStream`.
    pub async fn attach_raw(&self, id: &str) -> io::Result<tokio::net::UnixStream> {
        let mut stream = self.connect().await?;
        let req = Request::Attach {
            id: id.to_string(),
            raw: true,
        };
        let mut payload = serde_json::to_vec(&req)?;
        payload.push(b'\n');
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let mut first_line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed stream before handshake response",
                ));
            }
            if byte[0] == b'\n' {
                break;
            }
            first_line.push(byte[0]);
        }

        let resp: Response = serde_json::from_slice(&first_line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if !resp.ok {
            return Err(io::Error::other(
                resp.error.unwrap_or_else(|| "attach raw failed".into()),
            ));
        }

        Ok(stream)
    }
}

/// Handle to an attached session.
pub struct AttachedSession {
    pub id: String,
    pub scrollback: Vec<u8>,
    pub output_rx: mpsc::Receiver<Vec<u8>>,
    writer: Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    client: DaemonClient,
}

impl AttachedSession {
    /// Sends input data to the attached session.
    pub async fn send_input(&self, data: &[u8]) -> io::Result<()> {
        let b64 = base64::engine::general_purpose::STANDARD.encode(data);
        let req = Request::Input {
            id: self.id.clone(),
            data: b64,
        };
        let mut payload = serde_json::to_vec(&req)?;
        payload.push(b'\n');
        let mut guard = self.writer.lock().await;
        guard.write_all(&payload).await?;
        guard.flush().await
    }

    /// Resizes the attached session.
    pub async fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let req = Request::Resize {
            id: self.id.clone(),
            cols,
            rows,
            cell_w: None,
            cell_h: None,
        };
        let mut payload = serde_json::to_vec(&req)?;
        payload.push(b'\n');
        let mut guard = self.writer.lock().await;
        guard.write_all(&payload).await?;
        guard.flush().await
    }

    /// Closes the attached session.
    pub async fn close(&self) -> io::Result<()> {
        self.client.close_session(&self.id).await
    }

    /// Receives the next chunk of live output from the session.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        self.output_rx.recv().await
    }
}

// ============================================================================
// Synchronous Daemon Client
// ============================================================================

/// Synchronous client for interacting with the terminal daemon over a Unix domain socket.
#[derive(Debug, Clone)]
pub struct SyncDaemonClient {
    socket_path: PathBuf,
}

impl SyncDaemonClient {
    /// Creates a synchronous client targeting `socket_path` (or platform default).
    pub fn new(socket_path: Option<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.unwrap_or_else(default_socket_path),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(unix)]
    pub fn send_request(&self, req: &Request) -> io::Result<Response> {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut payload = serde_json::to_vec(req)?;
        payload.push(b'\n');
        stream.write_all(&payload)?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    #[cfg(not(unix))]
    pub fn send_request(&self, _req: &Request) -> io::Result<Response> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unix domain sockets are not supported on this platform",
        ))
    }

    /// Lists all sessions from the daemon.
    pub fn list_sessions(&self) -> io::Result<Vec<SessionInfo>> {
        let resp = self.send_request(&Request::List)?;
        if resp.ok {
            Ok(resp.sessions.unwrap_or_default())
        } else {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "list sessions failed".into()),
            ))
        }
    }

    /// Creates a persistent session on the daemon.
    pub fn create_session(
        &self,
        id: Option<String>,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
    ) -> io::Result<String> {
        self.create_session_with_options(id, cwd, cols, rows, None, None)
    }

    /// Creates a persistent session on the daemon with optional label and initial command.
    pub fn create_session_with_options(
        &self,
        id: Option<String>,
        cwd: Option<PathBuf>,
        cols: u16,
        rows: u16,
        label: Option<String>,
        command: Option<String>,
    ) -> io::Result<String> {
        let req = Request::Create {
            id,
            cwd,
            cols,
            rows,
            cell_w: None,
            cell_h: None,
            label,
            command,
        };
        let resp = self.send_request(&req)?;
        if resp.ok {
            resp.id
                .ok_or_else(|| io::Error::other("missing session id in response"))
        } else {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "create session failed".into()),
            ))
        }
    }

    /// Reads recent plain-text scrollback lines from a session without attaching a stream.
    pub fn read_session_text(&self, id: &str, lines: Option<u32>) -> io::Result<String> {
        let req = Request::Read {
            id: id.to_string(),
            lines,
        };
        let resp = self.send_request(&req)?;
        if resp.ok {
            Ok(resp.scrollback.unwrap_or_default())
        } else {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "read session failed".into()),
            ))
        }
    }

    /// Attaches to a persistent session in raw streaming mode, returning the UnixStream.
    #[cfg(unix)]
    pub fn attach_raw(&self, id: &str) -> io::Result<std::os::unix::net::UnixStream> {
        use std::io::{Read, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let req = Request::Attach {
            id: id.to_string(),
            raw: true,
        };
        let mut payload = serde_json::to_vec(&req)?;
        payload.push(b'\n');
        stream.write_all(&payload)?;
        stream.flush()?;

        // Read handshake response line byte-by-byte so no scrollback data is eaten:
        let mut line_bytes = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed stream before handshake response",
                ));
            }
            if byte[0] == b'\n' {
                break;
            }
            line_bytes.push(byte[0]);
        }

        let resp: Response = serde_json::from_slice(&line_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if !resp.ok {
            return Err(io::Error::other(
                resp.error.unwrap_or_else(|| "attach raw failed".into()),
            ));
        }

        // Disable timeouts for raw terminal streaming:
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;

        Ok(stream)
    }

    /// Closes a persistent session and terminates its process.
    pub fn close_session(&self, id: &str) -> io::Result<()> {
        let req = Request::Close { id: id.to_string() };
        let resp = self.send_request(&req)?;
        if resp.ok {
            Ok(())
        } else {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "close session failed".into()),
            ))
        }
    }

    /// Resizes a persistent session in the daemon.
    pub fn resize_session(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        let req = Request::Resize {
            id: id.to_string(),
            cols,
            rows,
            cell_w: None,
            cell_h: None,
        };
        let resp = self.send_request(&req)?;
        if resp.ok {
            Ok(())
        } else {
            Err(io::Error::other(
                resp.error.unwrap_or_else(|| "resize session failed".into()),
            ))
        }
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scrollback_buffer_basic_and_capacity() {
        let mut sb = ScrollbackBuffer::new(10);
        assert!(sb.is_empty());
        assert_eq!(sb.len(), 0);

        sb.push(b"hello");
        assert_eq!(sb.len(), 5);
        assert_eq!(sb.to_vec(), b"hello");

        sb.push(b" world");
        assert_eq!(sb.len(), 10);
        assert_eq!(sb.to_vec(), b"ello world");

        sb.push(b"!");
        assert_eq!(sb.len(), 10);
        assert_eq!(sb.to_vec(), b"llo world!");

        sb.push(b"0123456789extra");
        assert_eq!(sb.len(), 10);
        assert_eq!(sb.to_vec(), b"56789extra");

        sb.clear();
        assert!(sb.is_empty());
        assert_eq!(sb.to_vec(), b"");
    }

    #[test]
    fn test_protocol_serialization_roundtrip() {
        let req_create = Request::Create {
            id: Some("term-1".into()),
            cwd: Some(PathBuf::from("/tmp")),
            cols: 100,
            rows: 40,
            cell_w: Some(8),
            cell_h: Some(16),
            label: Some("worker-1".into()),
            command: Some("cargo test".into()),
        };
        let json_create = serde_json::to_string(&req_create).unwrap();
        assert!(json_create.contains(r#""cmd":"create""#));
        let decoded_create: Request = serde_json::from_str(&json_create).unwrap();
        assert_eq!(req_create, decoded_create);

        // Verify backward compatibility with legacy JSON without label/command
        let legacy_create = r#"{"cmd":"create","id":"term-legacy","cols":80,"rows":24}"#;
        let decoded_legacy: Request = serde_json::from_str(legacy_create).unwrap();
        assert_eq!(
            decoded_legacy,
            Request::Create {
                id: Some("term-legacy".into()),
                cwd: None,
                cols: 80,
                rows: 24,
                cell_w: None,
                cell_h: None,
                label: None,
                command: None,
            }
        );

        let req_read = Request::Read {
            id: "term-1".into(),
            lines: Some(50),
        };
        let json_read = serde_json::to_string(&req_read).unwrap();
        assert!(json_read.contains(r#""cmd":"read""#));
        let decoded_read: Request = serde_json::from_str(&json_read).unwrap();
        assert_eq!(req_read, decoded_read);

        let req_attach = Request::Attach {
            id: "term-1".into(),
            raw: true,
        };
        let json_attach = serde_json::to_string(&req_attach).unwrap();
        let decoded_attach: Request = serde_json::from_str(&json_attach).unwrap();
        assert_eq!(req_attach, decoded_attach);

        let req_input = Request::Input {
            id: "term-1".into(),
            data: "aGVsbG8=".into(),
        };
        let json_input = serde_json::to_string(&req_input).unwrap();
        let decoded_input: Request = serde_json::from_str(&json_input).unwrap();
        assert_eq!(req_input, decoded_input);

        let req_resize = Request::Resize {
            id: "term-1".into(),
            cols: 120,
            rows: 30,
            cell_w: None,
            cell_h: None,
        };
        let json_resize = serde_json::to_string(&req_resize).unwrap();
        let decoded_resize: Request = serde_json::from_str(&json_resize).unwrap();
        assert_eq!(req_resize, decoded_resize);

        let req_close = Request::Close {
            id: "term-1".into(),
        };
        let json_close = serde_json::to_string(&req_close).unwrap();
        let decoded_close: Request = serde_json::from_str(&json_close).unwrap();
        assert_eq!(req_close, decoded_close);

        let req_list = Request::List;
        let json_list = serde_json::to_string(&req_list).unwrap();
        let decoded_list: Request = serde_json::from_str(&json_list).unwrap();
        assert_eq!(req_list, decoded_list);

        let resp_success = Response::id("term-1".into());
        let json_resp = serde_json::to_string(&resp_success).unwrap();
        assert!(json_resp.contains(r#""ok":true"#));
        assert!(json_resp.contains(r#""id":"term-1""#));

        let evt_output = DaemonEvent::Output {
            data: "b3V0cHV0".into(),
        };
        let json_evt = serde_json::to_string(&evt_output).unwrap();
        assert!(json_evt.contains(r#""event":"output""#));
    }

    #[tokio::test]
    async fn raw_input_reaches_the_shell_while_output_is_backpressured() {
        let dir = std::env::temp_dir().join(format!("muqun-input-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("term.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = Arc::new(DaemonServer::new());
        let (shutdown, rx) = watch::channel(false);
        let worker = tokio::spawn(run_server_with_shutdown(server.clone(), listener, rx));
        let client = DaemonClient::new(Some(socket));
        let id = client
            .create_session(None, Some(dir.clone()), 80, 24)
            .await
            .unwrap();
        let mut stream = client.attach_raw(&id).await.unwrap();
        let session = server.sessions.lock().unwrap().get(&id).unwrap().clone();
        // Much larger than a Unix socket's send buffer. Never drain this client:
        // input must remain usable even while its output cannot make progress.
        session
            .broadcast_tx
            .send(OutputMsg::Data(vec![b'x'; 8 << 20]))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        stream
            .write_all(b"printf INPUT_OK > input-received\n")
            .await
            .unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), async {
            while std::fs::read(dir.join("input-received")).ok().as_deref() != Some(b"INPUT_OK") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        drop(stream);
        client.close_session(&id).await.unwrap();
        let _ = shutdown.send(true);
        worker.await.unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            received,
            "blocked output prevented the shell from receiving input"
        );
    }

    #[tokio::test]
    async fn test_daemon_server_and_client_lifecycle() {
        let socket_dir = std::env::temp_dir().join(format!("muqun-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket_path = socket_dir.join("test-term.sock");

        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = Arc::new(DaemonServer::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_clone = server.clone();
        let server_task = tokio::spawn(async move {
            run_server_with_shutdown(server_clone, listener, shutdown_rx)
                .await
                .unwrap();
        });

        let client = DaemonClient::new(Some(socket_path.clone()));

        // 1. Create session
        let session_id = client
            .create_session(Some("test-term-1".into()), None, 80, 24)
            .await
            .unwrap();
        assert_eq!(session_id, "test-term-1");

        // 2. List sessions
        let sessions = client.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "test-term-1");
        assert!(sessions[0].alive);
        assert_eq!(sessions[0].cols, 80);
        assert_eq!(sessions[0].rows, 24);

        // 3. Attach session and stream
        let mut attached = client.attach_session("test-term-1").await.unwrap();
        assert_eq!(attached.id, "test-term-1");

        // 4. Send input to shell
        attached.send_input(b"echo PERSISTENT_OK\n").await.unwrap();

        // 5. Read output until echo is observed
        let mut collected = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            if let Ok(Some(chunk)) =
                tokio::time::timeout(Duration::from_millis(500), attached.recv()).await
            {
                collected.extend_from_slice(&chunk);
                if collected
                    .windows(b"PERSISTENT_OK".len())
                    .any(|w| w == b"PERSISTENT_OK")
                {
                    break;
                }
            } else if collected
                .windows(b"PERSISTENT_OK".len())
                .any(|w| w == b"PERSISTENT_OK")
            {
                break;
            }
        }
        let output_str = String::from_utf8_lossy(&collected);
        assert!(
            output_str.contains("PERSISTENT_OK"),
            "shell output did not contain echo result: {output_str}"
        );

        // 6. Test reattachment and scrollback preservation
        drop(attached);
        let attached_second = client.attach_session("test-term-1").await.unwrap();
        let scrollback_str = String::from_utf8_lossy(&attached_second.scrollback);
        assert!(
            scrollback_str.contains("PERSISTENT_OK"),
            "reattached scrollback did not retain previous output: {scrollback_str}"
        );

        // 7. Resize session
        client.resize_session("test-term-1", 120, 35).await.unwrap();
        let sessions_after_resize = client.list_sessions().await.unwrap();
        assert_eq!(sessions_after_resize[0].cols, 120);
        assert_eq!(sessions_after_resize[0].rows, 35);

        // 8. Close session
        client.close_session("test-term-1").await.unwrap();
        let sessions_after_close = client.list_sessions().await.unwrap();
        assert!(sessions_after_close.is_empty());

        // Clean up server
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&socket_dir);
    }

    #[test]
    fn test_scrollback_buffer_edge_cases() {
        let mut zero_sb = ScrollbackBuffer::new(0);
        zero_sb.push(b"hello");
        assert!(zero_sb.is_empty());
        assert_eq!(zero_sb.to_vec(), b"");

        let mut exact_sb = ScrollbackBuffer::new(5);
        exact_sb.push(b"hello");
        assert_eq!(exact_sb.len(), 5);
        assert_eq!(exact_sb.to_vec(), b"hello");
        exact_sb.push(b"world");
        assert_eq!(exact_sb.len(), 5);
        assert_eq!(exact_sb.to_vec(), b"world");

        let mut empty_push_sb = ScrollbackBuffer::new(10);
        empty_push_sb.push(b"");
        assert!(empty_push_sb.is_empty());
    }

    #[tokio::test]
    async fn test_multiple_concurrent_attached_clients() {
        let socket_dir = std::env::temp_dir().join(format!("muqun-multi-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket_path = socket_dir.join("test-multi.sock");

        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = Arc::new(DaemonServer::new());
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let server_clone = server.clone();
        let server_task = tokio::spawn(async move {
            run_server_with_shutdown(server_clone, listener, shutdown_rx)
                .await
                .unwrap();
        });

        let client = DaemonClient::new(Some(socket_path.clone()));
        let session_id = client.create_session(None, None, 80, 24).await.unwrap();

        let mut client1 = client.attach_session(&session_id).await.unwrap();
        let mut client2 = client.attach_session(&session_id).await.unwrap();

        client1
            .send_input(b"echo DUAL_BROADCAST_OK\n")
            .await
            .unwrap();

        let mut out1 = Vec::new();
        let mut out2 = Vec::new();

        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(4) {
            tokio::select! {
                chunk = client1.recv() => {
                    if let Some(c) = chunk {
                        out1.extend_from_slice(&c);
                    }
                }
                chunk = client2.recv() => {
                    if let Some(c) = chunk {
                        out2.extend_from_slice(&c);
                    }
                }
            }
            let s1 = String::from_utf8_lossy(&out1);
            let s2 = String::from_utf8_lossy(&out2);
            if s1.contains("DUAL_BROADCAST_OK") && s2.contains("DUAL_BROADCAST_OK") {
                break;
            }
        }

        assert!(String::from_utf8_lossy(&out1).contains("DUAL_BROADCAST_OK"));
        assert!(String::from_utf8_lossy(&out2).contains("DUAL_BROADCAST_OK"));

        client.close_session(&session_id).await.unwrap();
        let _ = shutdown_tx.send(true);
        let _ = server_task.await;
        let _ = std::fs::remove_dir_all(&socket_dir);
    }

    #[test]
    fn test_sync_daemon_client_lifecycle() {
        let socket_dir = std::env::temp_dir().join(format!("muqun-sync-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket_path = socket_dir.join("test-sync.sock");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let sp = socket_path.clone();

        let server_task = rt.spawn(async move {
            let listener = tokio::net::UnixListener::bind(&sp).unwrap();
            let server = Arc::new(DaemonServer::new());
            run_server_with_shutdown(server, listener, shutdown_rx)
                .await
                .unwrap();
        });

        // Wait briefly for server ready
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if is_socket_alive(&socket_path) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let sync_client = SyncDaemonClient::new(Some(socket_path.clone()));
        let session_id = sync_client
            .create_session(Some("sync-test-1".into()), None, 80, 24)
            .unwrap();
        assert_eq!(session_id, "sync-test-1");

        let sessions = sync_client.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sync-test-1");

        let mut stream = sync_client.attach_raw(&session_id).unwrap();
        use std::io::{Read, Write};
        stream.write_all(b"echo SYNC_ECHO_OK\n").unwrap();

        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        let deadline = Instant::now() + Duration::from_secs(3);
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        while Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&out).contains("SYNC_ECHO_OK") {
                        break;
                    }
                }
                Err(ref e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
        assert!(String::from_utf8_lossy(&out).contains("SYNC_ECHO_OK"));

        sync_client.resize_session(&session_id, 100, 30).unwrap();
        sync_client.close_session(&session_id).unwrap();

        let sessions_after = sync_client.list_sessions().unwrap();
        assert!(sessions_after.is_empty());

        let _ = shutdown_tx.send(true);
        let _ = rt.block_on(server_task);
        let _ = std::fs::remove_dir_all(&socket_dir);
    }

    #[test]
    fn test_strip_ansi_codes_and_inspect_output_chunk() {
        // 1. Test strip_ansi_codes with CSI, OSC (BEL and ST), 2-byte ESC, and \r\n
        let raw = b"\x1b]7;file://localhost/tmp/work%20dir\x07\x1b[32mhello\x1b[0m \x1b]133;A\x1b\\world\r\nline2\x1bM\r\n";
        let stripped = strip_ansi_codes(raw);
        assert_eq!(stripped, "hello world\nline2\n");

        // 2. Test inspect_output_chunk with OSC 7 and OSC 133 (A, C, D;0, D;1)
        let cwd = Mutex::new(PathBuf::from("/initial"));
        let status = Mutex::new(Some("idle".to_string()));
        let exit_code = Mutex::new(None);
        let summary = Mutex::new(None);

        let evt_osc7 = inspect_output_chunk(
            b"\x1b]7;file://myhost/var/log/my%20app\x07",
            &cwd,
            &status,
            &exit_code,
            &summary,
        );
        assert_eq!(
            evt_osc7,
            Some(DaemonEvent::Status {
                cwd: Some(PathBuf::from("/var/log/my app")),
                agent_status: None,
                exit_code: None,
                task_summary: None,
            })
        );
        assert_eq!(*cwd.lock().unwrap(), PathBuf::from("/var/log/my app"));

        let evt_c = inspect_output_chunk(b"\x1b]133;C\x07", &cwd, &status, &exit_code, &summary);
        assert_eq!(
            evt_c,
            Some(DaemonEvent::Status {
                cwd: None,
                agent_status: Some("working".to_string()),
                exit_code: None,
                task_summary: None,
            })
        );
        assert_eq!(status.lock().unwrap().as_deref(), Some("working"));

        let evt_d0 =
            inspect_output_chunk(b"\x1b]133;D;0\x1b\\", &cwd, &status, &exit_code, &summary);
        assert_eq!(
            evt_d0,
            Some(DaemonEvent::Status {
                cwd: None,
                agent_status: Some("idle".to_string()),
                exit_code: Some(0),
                task_summary: None,
            })
        );
        assert_eq!(status.lock().unwrap().as_deref(), Some("idle"));
        assert_eq!(*exit_code.lock().unwrap(), Some(0));

        let evt_d1 = inspect_output_chunk(b"\x1b]133;D;1\x07", &cwd, &status, &exit_code, &summary);
        assert_eq!(
            evt_d1,
            Some(DaemonEvent::Status {
                cwd: None,
                agent_status: Some("idle".to_string()),
                exit_code: Some(1),
                task_summary: None,
            })
        );
        assert_eq!(*exit_code.lock().unwrap(), Some(1));

        // 3. Test placeholder ignored
        let evt_placeholder = inspect_output_chunk(
            b"MUQUN_TASK_DONE: <one-line summary>\nMUQUN_TASK_BLOCKED: <one-line reason>\n",
            &cwd,
            &status,
            &exit_code,
            &summary,
        );
        assert_eq!(evt_placeholder, None);

        // 4. Test MUQUN_TASK_DONE and verify OSC 133;A / 133;D do not overwrite "done"
        let evt_done = inspect_output_chunk(
            b"output line\r\nMUQUN_TASK_DONE: all tests pass\r\n\x1b]133;D;0\x07\x1b]133;A\x07",
            &cwd,
            &status,
            &exit_code,
            &summary,
        );
        assert_eq!(
            evt_done,
            Some(DaemonEvent::Status {
                cwd: None,
                agent_status: Some("done".to_string()),
                exit_code: Some(0),
                task_summary: Some("all tests pass".to_string()),
            })
        );
        assert_eq!(status.lock().unwrap().as_deref(), Some("done"));
        assert_eq!(summary.lock().unwrap().as_deref(), Some("all tests pass"));

        // Prompt redraw (\x1b]133;A) must not overwrite "done"
        let evt_a_after_done =
            inspect_output_chunk(b"\x1b]133;A\x07", &cwd, &status, &exit_code, &summary);
        assert_eq!(evt_a_after_done, None);
        assert_eq!(status.lock().unwrap().as_deref(), Some("done"));

        // 5. Test MUQUN_TASK_BLOCKED
        let evt_blocked = inspect_output_chunk(
            b"MUQUN_TASK_BLOCKED: need API key\n",
            &cwd,
            &status,
            &exit_code,
            &summary,
        );
        assert_eq!(
            evt_blocked,
            Some(DaemonEvent::Status {
                cwd: None,
                agent_status: Some("blocked".to_string()),
                exit_code: None,
                task_summary: Some("need API key".to_string()),
            })
        );
        assert_eq!(status.lock().unwrap().as_deref(), Some("blocked"));
        assert_eq!(summary.lock().unwrap().as_deref(), Some("need API key"));
    }

    #[test]
    fn test_create_session_with_options_and_read_session_text() {
        let socket_dir = std::env::temp_dir().join(format!("muqun-opt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let socket_path = socket_dir.join("test-options.sock");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let sp = socket_path.clone();

        let server_task = rt.spawn(async move {
            let listener = tokio::net::UnixListener::bind(&sp).unwrap();
            let server = Arc::new(DaemonServer::new());
            run_server_with_shutdown(server, listener, shutdown_rx)
                .await
                .unwrap();
        });

        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(500) {
            if is_socket_alive(&socket_path) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let sync_client = SyncDaemonClient::new(Some(socket_path));
        let session_id = sync_client
            .create_session_with_options(
                Some("worker-opt-1".into()),
                Some(socket_dir.clone()),
                80,
                24,
                Some("test-worker".into()),
                Some(
                    "printf '\\033[32m%s\\033[0m\\n%s: finished task\\n' READ_PLAIN_OK MUQUN_TASK_DONE"
                        .into(),
                ),
            )
            .unwrap();
        assert_eq!(session_id, "worker-opt-1");

        let deadline = Instant::now() + Duration::from_secs(4);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = sync_client
                .read_session_text(&session_id, Some(50))
                .unwrap();
            let sessions = sync_client.list_sessions().unwrap();
            let done = sessions.first().and_then(|s| s.agent_status.as_deref()) == Some("done");
            if text.contains("READ_PLAIN_OK") && done {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }

        assert!(
            text.contains("READ_PLAIN_OK"),
            "expected plain text output in scrollback read, got: {text}"
        );
        assert!(
            !text.contains("\x1b[32m"),
            "ANSI escape sequences should be stripped from read_session_text: {text}"
        );

        let sessions = sync_client.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].label.as_deref(), Some("test-worker"));
        assert_eq!(sessions[0].agent_status.as_deref(), Some("done"));
        assert_eq!(sessions[0].task_summary.as_deref(), Some("finished task"));

        sync_client.close_session(&session_id).unwrap();
        let _ = shutdown_tx.send(true);
        let _ = rt.block_on(server_task);
        let _ = std::fs::remove_dir_all(&socket_dir);
    }
}
