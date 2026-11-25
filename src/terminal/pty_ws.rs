//! `GET /api/pty/ws`: interactive remote PTY WebSocket for paired Desktop/App devices.
//!
//! Spawns the host owner's login shell inside a real pseudo-terminal (`openpty`)
//! and bridges raw terminal I/O and resize events over a WebSocket using the
//! same paired-device authentication and E2EE v2 per-frame sealing (`FrameCodec`)
//! as `GET /api/ws`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{
        ws::{
            close_code, rejection::WebSocketUpgradeRejection, CloseFrame, Message, WebSocket,
            WebSocketUpgrade,
        },
        Extension, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::agents::ws_routes::FrameCodec;
use crate::{
    api_error, require_device, still_paired, AppState, EncryptedStreamContext,
    STREAM_DEVICE_RECHECK_INTERVAL,
};

pub const PTY_WS_PATH: &str = "/api/pty/ws";
pub const PTY_WS_PROTOCOL: u64 = 1;

/// Largest inbound client frame (covers large pastes encoded as base64 JSON).
const MAX_INBOUND_FRAME_BYTES: usize = 256 * 1024;
/// Raw bytes read from the PTY per outbound frame.
const PTY_READ_CHUNK: usize = 32 * 1024;
/// Bounded back-pressure queue between PTY reader thread and WS writer.
const PTY_OUTPUT_QUEUE: usize = 64;
/// Bounded queue between WS reader and PTY writer thread.
const PTY_INPUT_QUEUE: usize = 256;
/// Max concurrent interactive PTYs per paired device.
const MAX_PTYS_PER_DEVICE: usize = 16;
/// Max concurrent interactive PTYs across all devices.
const MAX_PTYS_TOTAL: usize = 32;
/// How long to wait for final PTY output after the shell exits.
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(150);
/// Server heartbeat interval.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);
/// Outbound WebSocket frame write timeout.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default)]
pub(crate) struct PtyRegistry {
    active: Mutex<PtyCounts>,
}

#[derive(Debug, Default)]
struct PtyCounts {
    total: usize,
    per_device: HashMap<String, usize>,
}

pub(crate) struct PtySlot {
    registry: Arc<PtyRegistry>,
    device_id: String,
}

impl Drop for PtySlot {
    fn drop(&mut self) {
        let mut counts = self
            .registry
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        counts.total = counts.total.saturating_sub(1);
        if let Some(n) = counts.per_device.get_mut(&self.device_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_device.remove(&self.device_id);
            }
        }
    }
}

impl PtyRegistry {
    fn admit(self: &Arc<Self>, device_id: &str) -> Option<PtySlot> {
        let mut counts = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if counts.total >= MAX_PTYS_TOTAL {
            return None;
        }
        let dev = counts.per_device.get(device_id).copied().unwrap_or(0);
        if dev >= MAX_PTYS_PER_DEVICE {
            return None;
        }
        counts.total += 1;
        *counts.per_device.entry(device_id.to_owned()).or_insert(0) += 1;
        Some(PtySlot {
            registry: self.clone(),
            device_id: device_id.to_owned(),
        })
    }
}

pub(crate) fn mount(router: Router<AppState>) -> Router<AppState> {
    router.route(PTY_WS_PATH, get(upgrade))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WinSize {
    cols: u16,
    rows: u16,
    px_w: u16,
    px_h: u16,
}

impl WinSize {
    fn clamped(cols: u16, rows: u16, px_w: u16, px_h: u16) -> Self {
        Self {
            cols: cols.clamp(1, 512),
            rows: rows.clamp(1, 512),
            px_w,
            px_h,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PtyExit {
    Code(i32),
    Signal(i32),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    Open {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
        #[serde(default)]
        px_w: u16,
        #[serde(default)]
        px_h: u16,
    },
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        px_w: u16,
        #[serde(default)]
        px_h: u16,
    },
    Ping,
    Close,
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

fn resolve_cwd(requested: Option<&str>) -> PathBuf {
    let fallback = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let Some(raw) = requested.map(str::trim).filter(|s| !s.is_empty()) else {
        return fallback();
    };
    if raw.len() > 4096 || raw.chars().any(char::is_control) {
        return fallback();
    }
    let path = if raw == "~" {
        fallback()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        fallback().join(rest)
    } else {
        PathBuf::from(raw)
    };
    if path.is_dir() {
        std::fs::canonicalize(&path).unwrap_or(path)
    } else {
        fallback()
    }
}

async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    stream_crypto: Option<Extension<EncryptedStreamContext>>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !cfg!(unix) {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "pty_unsupported",
            "remote PTY is not supported on this platform",
        )
        .into_response();
    }
    let device_id = match require_device(&state, &headers) {
        Ok(id) => id,
        Err(refusal) => return refusal.into_response(),
    };
    let Ok(upgrade) = upgrade else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "websocket_upgrade_required",
            "this route only answers a WebSocket upgrade",
        )
        .into_response();
    };
    let connection_id = uuid::Uuid::new_v4().to_string();
    let codec = match FrameCodec::for_context(
        stream_crypto.as_ref().map(|Extension(context)| context),
        &connection_id,
    ) {
        Ok(codec) => codec,
        Err(_) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "transport_key_unavailable",
                "encrypted transport is unavailable",
            )
            .into_response();
        }
    };
    let Some(slot) = state.pty_connections.admit(&device_id) else {
        return api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_ptys",
            "too many open remote terminals",
        )
        .into_response();
    };

    upgrade
        .max_message_size(MAX_INBOUND_FRAME_BYTES)
        .max_frame_size(MAX_INBOUND_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            run_pty_connection(state, socket, codec, device_id, connection_id, slot).await;
        })
}

async fn send_frame(
    socket: &mut WebSocket,
    codec: &mut FrameCodec,
    plaintext: &str,
) -> Result<(), ()> {
    let wire = codec.encode(plaintext).map_err(|_| ())?;
    tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Text(wire.into())))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn close_socket(socket: &mut WebSocket) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        socket.send(Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: "".into(),
        }))),
    )
    .await;
}

async fn run_pty_connection(
    state: AppState,
    mut socket: WebSocket,
    mut codec: FrameCodec,
    device_id: String,
    connection_id: String,
    _slot: PtySlot,
) {
    let hello = json!({
        "t": "hello",
        "connection_id": connection_id,
        "protocol": PTY_WS_PROTOCOL,
    })
    .to_string();
    if send_frame(&mut socket, &mut codec, &hello).await.is_err() {
        return;
    }

    // Wait for the initial `open` frame (with a 15 s handshake timeout).
    let (cwd, initial_size) = loop {
        let next = match tokio::time::timeout(Duration::from_secs(15), socket.recv()).await {
            Ok(Some(Ok(msg))) => msg,
            _ => return,
        };
        let text = match next {
            Message::Text(t) => t.to_string(),
            Message::Ping(p) => {
                let _ = tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Pong(p))).await;
                continue;
            }
            Message::Pong(_) => continue,
            Message::Close(_) => return,
            _ => continue,
        };
        let opened = match codec.decode(&text) {
            Ok(s) => s,
            Err(err) => {
                let _ = send_frame(
                    &mut socket,
                    &mut codec,
                    &json!({"t": "error", "code": err.code(), "message": "invalid frame"})
                        .to_string(),
                )
                .await;
                close_socket(&mut socket).await;
                return;
            }
        };
        match serde_json::from_str::<ClientMsg>(&opened) {
            Ok(ClientMsg::Open {
                cwd,
                cols,
                rows,
                px_w,
                px_h,
            }) => {
                break (
                    resolve_cwd(cwd.as_deref()),
                    WinSize::clamped(cols, rows, px_w, px_h),
                )
            }
            Ok(ClientMsg::Ping) => {
                if send_frame(&mut socket, &mut codec, r#"{"t":"pong"}"#)
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(ClientMsg::Close) => {
                close_socket(&mut socket).await;
                return;
            }
            _ => {
                let _ = send_frame(
                    &mut socket,
                    &mut codec,
                    &json!({"t": "error", "code": "open_required", "message": "first frame must be open"})
                        .to_string(),
                )
                .await;
                close_socket(&mut socket).await;
                return;
            }
        }
    };

    #[cfg(unix)]
    run_unix_pty(
        state,
        &mut socket,
        &mut codec,
        &device_id,
        cwd,
        initial_size,
    )
    .await;

    #[cfg(not(unix))]
    {
        let _ = (state, device_id, cwd, initial_size);
        let _ = send_frame(
            &mut socket,
            &mut codec,
            &json!({"t": "error", "code": "pty_unsupported", "message": "unsupported platform"})
                .to_string(),
        )
        .await;
        close_socket(&mut socket).await;
    }
}

#[cfg(unix)]
enum PtyWrite {
    Bytes(Vec<u8>),
    Resize(WinSize),
}

#[cfg(unix)]
async fn run_unix_pty(
    state: AppState,
    socket: &mut WebSocket,
    codec: &mut FrameCodec,
    device_id: &str,
    cwd: PathBuf,
    initial_size: WinSize,
) {
    let (exit_tx, mut exit_rx) = mpsc::channel::<PtyExit>(1);
    let (pty, _child) = match unix::spawn_shell(
        &cwd,
        initial_size,
        Box::new(move |exit| {
            let _ = exit_tx.blocking_send(exit);
        }),
    ) {
        Ok(pair) => pair,
        Err(err) => {
            let _ = send_frame(
                socket,
                codec,
                &json!({"t": "error", "code": "pty_spawn_failed", "message": err.to_string()})
                    .to_string(),
            )
            .await;
            close_socket(socket).await;
            return;
        }
    };

    let mut reader = match pty.reader() {
        Ok(r) => r,
        Err(err) => {
            let _ = send_frame(
                socket,
                codec,
                &json!({"t": "error", "code": "pty_io_failed", "message": err.to_string()})
                    .to_string(),
            )
            .await;
            close_socket(socket).await;
            return;
        }
    };

    let opened = json!({
        "t": "opened",
        "cwd": cwd.to_string_lossy(),
    })
    .to_string();
    if send_frame(socket, codec, &opened).await.is_err() {
        return;
    }

    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(PTY_OUTPUT_QUEUE);
    let _read_thread = std::thread::Builder::new()
        .name("muqun-gw-pty-read".into())
        .spawn(move || {
            use std::io::Read as _;
            let mut buf = vec![0u8; PTY_READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

    let (in_tx, mut in_rx) = mpsc::channel::<PtyWrite>(PTY_INPUT_QUEUE);
    let _write_thread = std::thread::Builder::new()
        .name("muqun-gw-pty-write".into())
        .spawn(move || {
            let mut pty = pty;
            while let Some(cmd) = in_rx.blocking_recv() {
                match cmd {
                    PtyWrite::Bytes(bytes) => {
                        if pty.write_all(&bytes).is_err() {
                            return;
                        }
                    }
                    PtyWrite::Resize(size) => pty.resize(size),
                }
            }
        });

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;

    let mut recheck = tokio::time::interval(STREAM_DEVICE_RECHECK_INTERVAL);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    recheck.tick().await;

    let mut missed_pongs: u8 = 0;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if missed_pongs >= 2 {
                    break;
                }
                missed_pongs += 1;
                if tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Ping(Vec::new().into())))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            _ = recheck.tick() => {
                if !still_paired(&state, device_id) {
                    let _ = send_frame(
                        socket,
                        codec,
                        &json!({"t": "error", "code": "device_revoked", "message": "device is no longer paired"})
                            .to_string(),
                    )
                    .await;
                    break;
                }
            }
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(chunk) => {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&chunk);
                        let frame = json!({"t": "output", "data": encoded}).to_string();
                        if send_frame(socket, codec, &frame).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        if let Ok(Some(exit)) =
                            tokio::time::timeout(EXIT_DRAIN_GRACE, exit_rx.recv()).await
                        {
                            let _ = send_exit_frame(socket, codec, exit).await;
                        }
                        break;
                    }
                }
            }
            maybe_exit = exit_rx.recv() => {
                if let Some(exit) = maybe_exit {
                    // Drain any remaining buffered output briefly before sending exit.
                    let deadline = tokio::time::Instant::now() + EXIT_DRAIN_GRACE;
                    while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, out_rx.recv()).await {
                        let encoded = base64::engine::general_purpose::STANDARD.encode(&chunk);
                        let frame = json!({"t": "output", "data": encoded}).to_string();
                        if send_frame(socket, codec, &frame).await.is_err() {
                            return;
                        }
                    }
                    let _ = send_exit_frame(socket, codec, exit).await;
                    break;
                }
            }
            incoming = socket.recv() => {
                let Some(Ok(msg)) = incoming else {
                    break;
                };
                missed_pongs = 0;
                match msg {
                    Message::Text(text) => {
                        let opened = match codec.decode(&text) {
                            Ok(s) => s,
                            Err(err) => {
                                let _ = send_frame(
                                    socket,
                                    codec,
                                    &json!({"t": "error", "code": err.code(), "message": "invalid frame"})
                                        .to_string(),
                                )
                                .await;
                                break;
                            }
                        };
                        match serde_json::from_str::<ClientMsg>(&opened) {
                            Ok(ClientMsg::Input { data }) => {
                                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                                    if !bytes.is_empty() && in_tx.send(PtyWrite::Bytes(bytes)).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Ok(ClientMsg::Resize { cols, rows, px_w, px_h }) => {
                                let size = WinSize::clamped(cols, rows, px_w, px_h);
                                if in_tx.send(PtyWrite::Resize(size)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(ClientMsg::Ping) => {
                                if send_frame(socket, codec, r#"{"t":"pong"}"#).await.is_err() {
                                    break;
                                }
                            }
                            Ok(ClientMsg::Close) => break,
                            Ok(ClientMsg::Open { .. }) | Err(_) => {}
                        }
                    }
                    Message::Ping(p) => {
                        if tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Pong(p)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Binary(_) => {}
                }
            }
        }
    }

    drop(in_tx);
    close_socket(socket).await;
}

#[cfg(unix)]
async fn send_exit_frame(
    socket: &mut WebSocket,
    codec: &mut FrameCodec,
    exit: PtyExit,
) -> Result<(), ()> {
    let payload = match exit {
        PtyExit::Code(code) => json!({"t": "exit", "code": code}),
        PtyExit::Signal(signal) => json!({"t": "exit", "signal": signal}),
    };
    send_frame(socket, codec, &payload.to_string()).await
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    use super::{PtyExit, WinSize};

    pub(super) type OnExit = Box<dyn FnOnce(PtyExit) + Send>;

    fn user_shell() -> PathBuf {
        if let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
            return shell.into();
        }
        // SAFETY: getpwuid returns a pointer into static storage or null; it is
        // read immediately on this thread.
        unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if !pw.is_null() && !(*pw).pw_shell.is_null() {
                let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
                if let Ok(shell) = shell.to_str() {
                    if !shell.is_empty() {
                        return shell.into();
                    }
                }
            }
        }
        "/bin/sh".into()
    }

    fn winsize(size: WinSize) -> libc::winsize {
        libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.px_w,
            ws_ypixel: size.px_h,
        }
    }

    pub(super) struct Pty {
        master: File,
        _stop: io::PipeWriter,
        stopped: io::PipeReader,
    }

    pub(super) struct Reader {
        master: File,
        stopped: io::PipeReader,
    }

    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut fds = [
                libc::pollfd {
                    fd: self.master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stopped.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                // SAFETY: poll reads and writes the two stack pollfds above.
                let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if rc < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if fds[1].revents != 0 {
                    return Ok(0);
                }
                if fds[0].revents != 0 {
                    return self.master.read(buf);
                }
            }
        }
    }

    impl Pty {
        pub(super) fn reader(&self) -> io::Result<Reader> {
            Ok(Reader {
                master: self.master.try_clone()?,
                stopped: self.stopped.try_clone()?,
            })
        }

        pub(super) fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.master.write_all(bytes)
        }

        pub(super) fn resize(&self, size: WinSize) {
            let ws = winsize(size);
            // SAFETY: TIOCSWINSZ reads one `winsize` from the pointer.
            unsafe {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
            }
        }
    }

    pub(super) struct Child {
        pid: libc::pid_t,
        reaped: Arc<Mutex<bool>>,
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let reaped = self.reaped.lock().unwrap_or_else(|e| e.into_inner());
            if !*reaped {
                let pid = self.pid;
                let flag = self.reaped.clone();
                // SAFETY: sending SIGHUP to our own unreaped child process group.
                unsafe {
                    libc::kill(-pid, libc::SIGHUP);
                    libc::kill(pid, libc::SIGHUP);
                }
                drop(reaped);
                // Escalate to SIGKILL after 2s if the shell ignores SIGHUP or is blocked,
                // preventing orphaned PTY children and leaked wait threads.
                let _ = std::thread::Builder::new()
                    .name("muqun-gw-pty-kill".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        let reaped = flag.lock().unwrap_or_else(|e| e.into_inner());
                        if !*reaped {
                            // SAFETY: child is still unreaped, so pid has not been recycled.
                            unsafe {
                                libc::kill(-pid, libc::SIGKILL);
                                libc::kill(pid, libc::SIGKILL);
                            }
                        }
                    });
            }
        }
    }

    impl Child {
        fn watch(pid: libc::pid_t, on_exit: OnExit) -> io::Result<Self> {
            let reaped = Arc::new(Mutex::new(false));
            let flag = reaped.clone();
            std::thread::Builder::new()
                .name("muqun-gw-pty-wait".into())
                .spawn(move || {
                    loop {
                        // SAFETY: waitid fills the zeroed siginfo_t without reaping yet.
                        let rc = unsafe {
                            let mut info: libc::siginfo_t = std::mem::zeroed();
                            libc::waitid(
                                libc::P_PID,
                                pid as libc::id_t,
                                &mut info,
                                libc::WEXITED | libc::WNOWAIT,
                            )
                        };
                        if rc == 0
                            || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                        {
                            break;
                        }
                    }
                    let mut status = 0;
                    {
                        let mut reaped = flag.lock().unwrap_or_else(|e| e.into_inner());
                        // SAFETY: reaps our own exited child.
                        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
                            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                                break;
                            }
                        }
                        *reaped = true;
                    }
                    on_exit(if libc::WIFSIGNALED(status) {
                        PtyExit::Signal(libc::WTERMSIG(status))
                    } else {
                        PtyExit::Code(libc::WEXITSTATUS(status))
                    });
                })?;
            Ok(Self { pid, reaped })
        }
    }

    pub(super) fn spawn_shell(
        cwd: &Path,
        size: WinSize,
        on_exit: OnExit,
    ) -> io::Result<(Pty, Child)> {
        let shell = user_shell();
        let name = shell
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sh".into());
        let mut cmd = Command::new(&shell);
        cmd.arg0(format!("-{name}")).current_dir(cwd);
        spawn(cmd, size, on_exit)
    }

    fn spawn(mut cmd: Command, size: WinSize, on_exit: OnExit) -> io::Result<(Pty, Child)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        #[cfg(target_os = "macos")]
        let mut ws = winsize(size);
        #[cfg(not(target_os = "macos"))]
        let ws = winsize(size);
        // SAFETY: openpty writes two fds; name and termios may be null.
        let rc = unsafe {
            #[cfg(target_os = "macos")]
            let winp = &mut ws as *mut libc::winsize;
            #[cfg(not(target_os = "macos"))]
            let winp = &ws as *const libc::winsize;

            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                winp,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty succeeded, so both are fresh owned fds.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: mark both master and slave FD_CLOEXEC so no stray copies leak.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        cmd.env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "muqun-gateway")
            .env_remove("TERM_PROGRAM_VERSION")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let (stopped, stop) = io::pipe()?;
        let child = cmd.spawn()?;
        let pid = child.id() as libc::pid_t;
        drop(cmd);
        Ok((
            Pty {
                master: File::from(master),
                _stop: stop,
                stopped,
            },
            Child::watch(pid, on_exit)?,
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{serve, test_state};
    use crate::DeviceRecord;
    use futures::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tokio_tungstenite::tungstenite::Message as ClientMsgWs;

    fn device(token: &str) -> DeviceRecord {
        DeviceRecord {
            id: "dev-pty-1".into(),
            name: "Desktop".into(),
            token_hash: crate::hash_token(token),
            paired_unix_ms: 1,
            last_seen_unix_ms: 1,
            install_id: None,
            server_id: None,
            transport_key: None,
            kind: crate::connectivity::authority::DeviceKind::App,
        }
    }

    #[tokio::test]
    async fn pty_ws_spawns_shell_streams_io_and_reports_exit() {
        let state = test_state("admin", vec![device("pty-token")]);
        let app = mount(Router::new()).with_state(state.clone());
        let base = serve(app).await;
        let ws_url = format!("{}{}", base.replace("http://", "ws://"), PTY_WS_PATH);

        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer pty-token".parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

        // 1. Expect hello
        let hello: serde_json::Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(hello["t"], "hello");

        // 2. Open PTY in /tmp
        ws.send(ClientMsgWs::text(
            json!({"t": "open", "cwd": "/tmp", "cols": 80, "rows": 24}).to_string(),
        ))
        .await
        .unwrap();
        let opened: serde_json::Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(opened["t"], "opened");

        // 3. Resize and run echo + exit
        ws.send(ClientMsgWs::text(
            json!({"t": "resize", "cols": 100, "rows": 30}).to_string(),
        ))
        .await
        .unwrap();
        let cmd = b"printf 'MUQUN_REMOTE_PTY_OK\\n'; exit 0\n";
        ws.send(ClientMsgWs::text(
            json!({
                "t": "input",
                "data": base64::engine::general_purpose::STANDARD.encode(cmd),
            })
            .to_string(),
        ))
        .await
        .unwrap();

        let mut output = String::new();
        let mut saw_exit = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if let ClientMsgWs::Text(text) = msg {
                let val: serde_json::Value = serde_json::from_str(&text).unwrap();
                match val["t"].as_str() {
                    Some("output") => {
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(val["data"].as_str().unwrap())
                            .unwrap();
                        output.push_str(&String::from_utf8_lossy(&raw));
                    }
                    Some("exit") => {
                        saw_exit = val["code"].as_i64();
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            output.contains("MUQUN_REMOTE_PTY_OK"),
            "expected marker in remote PTY output, got: {output:?}"
        );
        assert_eq!(saw_exit, Some(0));
    }

    #[tokio::test]
    async fn pty_ws_rejects_unauthenticated_upgrade() {
        let state = test_state("admin", vec![device("pty-token")]);
        let app = mount(Router::new()).with_state(state);
        let base = serve(app).await;
        let ws_url = format!("{}{}", base.replace("http://", "ws://"), PTY_WS_PATH);
        let err = tokio_tungstenite::connect_async(ws_url).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") || msg.contains("Unauthorized"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn pty_ws_streams_over_e2ee_v2_transport_middleware() {
        use crate::connectivity::transport;
        use crate::platform::server::encrypted_transport;
        use axum::middleware;

        let material = [0x42u8; 32];
        let transport_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(material);
        let mut dev = device("pty-e2ee-token");
        dev.id = "dev-pty-e2ee".into();
        dev.transport_key = Some(transport_key);
        let state = test_state("admin", vec![dev]);
        let app = mount(Router::new())
            .layer(middleware::from_fn_with_state(
                state.clone(),
                encrypted_transport,
            ))
            .with_state(state);
        let base = serve(app).await;
        let ws_url = format!("{}{}", base.replace("http://", "ws://"), PTY_WS_PATH);

        let request_aad = format!("GET {PTY_WS_PATH}");
        let payload = json!({
            "token": "pty-e2ee-token",
            "content_type": null,
            "body": "",
        });
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let envelope = transport::seal_version(
            transport::VERSION_V2,
            &material,
            transport::Direction::Request,
            request_aad.as_bytes(),
            &serde_json::to_vec(&payload).unwrap(),
            now_ms,
        )
        .unwrap();
        let sealed_header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&envelope).unwrap());

        let mut req = ws_url.into_client_request().unwrap();
        req.headers_mut()
            .insert("x-muqun-transport", "1".parse().unwrap());
        req.headers_mut()
            .insert("x-muqun-device", "dev-pty-e2ee".parse().unwrap());
        req.headers_mut()
            .insert("x-muqun-envelope", sealed_header.parse().unwrap());

        let (mut ws, resp) = tokio_tungstenite::connect_async(req).await.unwrap();
        assert_eq!(
            resp.headers()
                .get("x-muqun-transport")
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );

        // 1. First server frame is sealed and carries `cid` + `seq: 0`.
        let first_wire: serde_json::Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        let cid = first_wire["cid"].as_str().unwrap().to_owned();
        assert_eq!(first_wire["seq"], 0);
        let server_key = transport::derive_ws_server_key(&material, &cid, &envelope.nonce).unwrap();
        let client_key = transport::derive_ws_client_key(&material, &cid, &envelope.nonce).unwrap();
        let mut recv_seq = 0u64;
        let mut send_seq = 0u64;

        let open_frame = |frame: &serde_json::Value, seq: &mut u64| -> serde_json::Value {
            assert_eq!(frame["seq"].as_u64(), Some(*seq));
            let s = *seq;
            let aad = format!("{request_aad}\n{cid}\n{s}");
            let pt = transport::open_stream_event(
                &server_key,
                s,
                aad.as_bytes(),
                frame["c"].as_str().unwrap(),
            )
            .unwrap();
            *seq += 1;
            serde_json::from_slice(&pt).unwrap()
        };
        let seal_frame = |plain: &str, seq: &mut u64| -> String {
            let s = *seq;
            let aad = format!("{request_aad}\n{cid}\n{s}");
            let c = transport::seal_stream_event(&client_key, s, aad.as_bytes(), plain.as_bytes())
                .unwrap();
            *seq += 1;
            json!({ "seq": s, "c": c }).to_string()
        };

        let hello = open_frame(&first_wire, &mut recv_seq);
        assert_eq!(hello["t"], "hello");

        // 2. Send sealed open + resize + input command.
        ws.send(ClientMsgWs::text(seal_frame(
            &json!({"t": "open", "cwd": "/tmp", "cols": 80, "rows": 24}).to_string(),
            &mut send_seq,
        )))
        .await
        .unwrap();
        let opened_wire: serde_json::Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        let opened = open_frame(&opened_wire, &mut recv_seq);
        assert_eq!(opened["t"], "opened");

        ws.send(ClientMsgWs::text(seal_frame(
            &json!({"t": "resize", "cols": 111, "rows": 33}).to_string(),
            &mut send_seq,
        )))
        .await
        .unwrap();
        let cmd = b"printf 'E2EE_PTY_OK '; stty size; exit 0\n";
        ws.send(ClientMsgWs::text(seal_frame(
            &json!({
                "t": "input",
                "data": base64::engine::general_purpose::STANDARD.encode(cmd),
            })
            .to_string(),
            &mut send_seq,
        )))
        .await
        .unwrap();

        let mut output = String::new();
        let mut saw_exit = None;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if let ClientMsgWs::Text(text) = msg {
                let wire: serde_json::Value = serde_json::from_str(&text).unwrap();
                let val = open_frame(&wire, &mut recv_seq);
                match val["t"].as_str() {
                    Some("output") => {
                        let raw = base64::engine::general_purpose::STANDARD
                            .decode(val["data"].as_str().unwrap())
                            .unwrap();
                        output.push_str(&String::from_utf8_lossy(&raw));
                    }
                    Some("exit") => {
                        saw_exit = val["code"].as_i64();
                        break;
                    }
                    _ => {}
                }
            }
        }
        assert!(
            output.contains("E2EE_PTY_OK"),
            "expected E2EE_PTY_OK in output, got: {output:?}"
        );
        assert!(
            output.contains("33 111"),
            "expected resized stty size '33 111' in output, got: {output:?}"
        );
        assert_eq!(saw_exit, Some(0));
    }
}
