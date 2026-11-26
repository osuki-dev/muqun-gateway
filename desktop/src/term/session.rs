//! One live terminal: the shell in a pty, a reader thread, and the emulator
//! thread that owns the [`Engine`] (libghostty types never leave it).
//!
//! The UI talks to it with [`Msg`]s and takes frames from a latest-wins slot;
//! the emulator wakes the window when it publishes one, at most every
//! [`FRAME_GAP`], so a flood (`yes`, `cat` of a big file) costs the UI one
//! frame per display refresh however fast the shell writes.

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::engine::{Colors, Engine, MouseInput, Size};
use super::grid::Frame;
use super::input::{KeyInput, Mods};
use crate::platform::pty::{self, Exit, WinSize};

/// The shortest time between two published frames.
const FRAME_GAP: Duration = Duration::from_millis(8);
/// Bytes read from the shell but not yet parsed, at most: past this the
/// reader stops reading and the shell blocks on its writes.
const MAX_IN_FLIGHT: usize = 4 << 20;
/// Once the shell has ended, how long its last words may take to arrive
/// (a job it left behind may hold the terminal open for good).
const LAST_WORDS: Duration = Duration::from_millis(150);

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The shell exited, or a signal killed it.
    Shell(Exit),
    /// The emulator could not start, or the shell stopped taking input.
    Broken,
}

pub enum Msg {
    Key(KeyInput),
    Text(String),
    Paste(String),
    Mouse(MouseInput),
    Wheel {
        lines: i32,
        x: f32,
        y: f32,
        mods: Mods,
    },
    Resize(Size),
    Colors(Colors),
    Bytes(Vec<u8>),
    /// Drop the history and have the shell redraw its prompt (Ctrl+L).
    Clear,
    /// Scroll the viewport to row `row` in the full buffer.
    ScrollToRow(usize),
    /// Scroll the viewport back to the bottom.
    ScrollToBottom,
    /// Request full plain text dump of terminal buffer (including scrollback).
    DumpText(std::sync::mpsc::Sender<String>),
    Eof,
    /// The shell ended.
    Exited(Exit),
    /// The panel closed: hang the shell up and stop.
    Shutdown,
}

/// Bytes between the reader and the emulator, for back-pressure.
#[derive(Default)]
struct InFlight {
    bytes: Mutex<usize>,
    drained: Condvar,
}

/// Lets the reader go, however full the pipe, when the emulator stops: it
/// then finds its pty gone and ends too.
struct Release(Arc<InFlight>);

impl Drop for Release {
    fn drop(&mut self) {
        *self.0.bytes.lock().unwrap_or_else(|e| e.into_inner()) = 0;
        self.0.drained.notify_all();
    }
}

struct SessionInner {
    tx: mpsc::Sender<Msg>,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
    latest: Arc<Mutex<Option<Frame>>>,
    ended: Arc<Mutex<Option<Ended>>>,
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    /// Remote computer ID when backed by `GET /api/pty/ws`, else `None` (local).
    pub remote_computer: Option<String>,
    /// When the shell started.
    pub started: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
            .field("remote_computer", &self.remote_computer)
            .field("started", &self.started)
            .finish()
    }
}

impl Session {
    /// Start the owner's shell in `cwd`; `wake` asks the window for a frame.
    /// Prefers the persistent headless terminal daemon when available on Unix,
    /// falling back to a direct in-process PTY shell.
    pub fn start(
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        Self::start_with_id(None, cwd, size, colors, wake)
    }

    /// Start a new or reattach to an existing persistent session by id.
    pub fn start_with_id(
        session_id: Option<String>,
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            if let Ok(session) =
                Self::start_daemon(session_id.clone(), cwd.clone(), size, colors, wake.clone())
            {
                return Ok(session);
            }
        }
        Self::start_on(cwd, size, colors, wake, |cwd, ws, on_exit| {
            pty::spawn_shell(cwd, ws, on_exit)
        })
    }

    /// `cmd` instead of the owner's shell, through the same pty.
    #[cfg(all(test, unix))]
    pub fn start_command(
        cmd: std::process::Command,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        Self::start_on(PathBuf::from("/"), size, colors, wake, |_, ws, on_exit| {
            pty::spawn_command(cmd, ws, on_exit)
        })
    }

    #[cfg(unix)]
    fn start_daemon(
        session_id: Option<String>,
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        use super::daemon::{ensure_daemon_running, SyncDaemonClient};
        use std::io::Write;

        let socket_path = ensure_daemon_running()?;
        let client = SyncDaemonClient::new(Some(socket_path));
        let is_reattach = session_id.is_some();
        let id = match session_id {
            Some(existing) => existing,
            None => client.create_session_with_size(
                None,
                Some(cwd.clone()),
                size.cols,
                size.rows,
                u16::try_from(size.cell_w).ok(),
                u16::try_from(size.cell_h).ok(),
            )?,
        };
        let (mut stream, declared_scrollback_len) = client.attach_raw_with_len(&id)?;
        let mut initial_scrollback = Vec::new();
        if is_reattach {
            const MAX_INITIAL_REPLAY: usize = super::daemon::DEFAULT_SCROLLBACK_CAPACITY + 4096;
            if let Some(expected) = declared_scrollback_len {
                let target = expected.min(MAX_INITIAL_REPLAY);
                if target > 0 {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    initial_scrollback.resize(target, 0);
                    let mut filled = 0;
                    while filled < target {
                        match stream.read(&mut initial_scrollback[filled..]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => break,
                        }
                    }
                    initial_scrollback.truncate(filled);
                    let _ = stream.set_read_timeout(None);
                }
            } else {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
                let mut sb_buf = [0u8; 64 * 1024];
                loop {
                    match stream.read(&mut sb_buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            initial_scrollback.extend_from_slice(&sb_buf[..n]);
                            if initial_scrollback.len() >= MAX_INITIAL_REPLAY {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                let _ = stream.set_read_timeout(None);
            }
        }
        let mut reader_stream = stream.try_clone()?;
        let mut writer_stream = stream;

        let (tx, rx) = mpsc::channel::<Msg>();
        let latest = Arc::new(Mutex::new(None));
        let ended = Arc::new(Mutex::new(None));
        let in_flight = Arc::new(InFlight::default());

        {
            let tx = tx.clone();
            let in_flight = in_flight.clone();
            std::thread::Builder::new()
                .name("muqun-term-read".into())
                .spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        {
                            let mut n = in_flight.bytes.lock().unwrap();
                            while *n > MAX_IN_FLIGHT {
                                n = in_flight.drained.wait(n).unwrap();
                            }
                        }
                        match reader_stream.read(&mut buf) {
                            Ok(0) | Err(_) => {
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                            Ok(n) => {
                                *in_flight.bytes.lock().unwrap() += n;
                                if tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                })?;
        }

        {
            let latest = latest.clone();
            let ended = ended.clone();
            let client_clone = client.clone();
            let id_clone = id.clone();
            std::thread::Builder::new()
                .name("muqun-term".into())
                .spawn(move || {
                    let _release = Release(in_flight.clone());
                    let end = |how: Ended| {
                        *ended.lock().unwrap() = Some(how);
                        wake();
                    };
                    let mut engine = match Engine::new(size, colors) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::warn!("terminal: could not start the emulator: {err}");
                            end(Ended::Broken);
                            return;
                        }
                    };
                    if !initial_scrollback.is_empty() {
                        if initial_scrollback.len() >= 1024 * 1024 {
                            // Ring buffer wrapped and may begin mid-sequence; reset SGR first.
                            engine.feed(b"\x1b[0m");
                        }
                        engine.feed(&initial_scrollback);
                        // Discard historical terminal query replies (DECRQM, DA1, DSR, Kitty)
                        // so reattaching never injects escape sequences into the live PTY stdin.
                        let _ = engine.take_output();
                        if let Ok(Some(frame)) = engine.frame() {
                            *latest.lock().unwrap() = Some(frame);
                            wake();
                        }
                    }
                    // On reattach, schedule a spaced two-step SIGWINCH redraw (cols -> cols-1,
                    // wait 90ms, -> cols) if no dimension-changing viewport resize arrives first.
                    // Back-to-back TIOCSWINSZ ioctls get coalesced into a single SIGWINCH where
                    // full-screen TUIs (nvim, htop) observe unchanged dimensions and skip repainting
                    // over truncated/wrapped scrollback replay.
                    let mut reattach_nudge_at =
                        is_reattach.then(|| Instant::now() + Duration::from_millis(60));
                    let mut nudge_restore: Option<(Instant, Size)> = None;
                    let mut last = Instant::now() - FRAME_GAP;
                    let mut pending = false;
                    let mut how: Option<Ended> = None;
                    let mut until = Instant::now();

                    loop {
                        let now = Instant::now();
                        let mut nudged = false;
                        if let Some((restore_at, target)) = nudge_restore {
                            if now >= restore_at {
                                nudge_restore = None;
                                if engine.size() != target {
                                    log(engine.resize(target));
                                    let _ = client_clone.resize_session_with_cells(
                                        &id_clone,
                                        target.cols,
                                        target.rows,
                                        u16::try_from(target.cell_w).ok(),
                                        u16::try_from(target.cell_h).ok(),
                                    );
                                    nudged = true;
                                    pending = true;
                                }
                            }
                        } else if let Some(nudge_at) = reattach_nudge_at {
                            if now >= nudge_at {
                                reattach_nudge_at = None;
                                let cur = engine.size();
                                let nudge = Size {
                                    cols: if cur.cols > 2 {
                                        cur.cols - 1
                                    } else {
                                        cur.cols + 1
                                    },
                                    ..cur
                                };
                                log(engine.resize(nudge));
                                let _ = client_clone.resize_session_with_cells(
                                    &id_clone,
                                    nudge.cols,
                                    nudge.rows,
                                    u16::try_from(nudge.cell_w).ok(),
                                    u16::try_from(nudge.cell_h).ok(),
                                );
                                nudge_restore = Some((now + Duration::from_millis(90), cur));
                                nudged = true;
                                pending = true;
                            }
                        }
                        if nudged {
                            // Flush any in-band resize report (Mode 2048, `\x1b[48;...t`)
                            // immediately so TUIs like Neovim 0.10+ repaint without waiting
                            // for the next recv timeout.
                            let out = engine.take_output();
                            if !out.is_empty()
                                && how.is_none()
                                && writer_stream.write_all(&out).is_err()
                            {
                                how = Some(Ended::Broken);
                            }
                        }

                        let mut wake_at: Option<Instant> = None;
                        let mut consider = |t: Instant| {
                            wake_at = Some(match wake_at {
                                Some(cur) => cur.min(t),
                                None => t,
                            });
                        };
                        if pending {
                            consider(last + FRAME_GAP);
                        }
                        if how.is_some() {
                            consider(until);
                        }
                        if let Some(t) = reattach_nudge_at {
                            consider(t);
                        }
                        if let Some((t, _)) = nudge_restore {
                            consider(t);
                        }

                        let msg = if let Some(deadline) = wake_at {
                            let wait = deadline.saturating_duration_since(Instant::now());
                            match rx.recv_timeout(wait) {
                                Ok(m) => Some(m),
                                Err(RecvTimeoutError::Timeout) => None,
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        } else {
                            match rx.recv() {
                                Ok(m) => Some(m),
                                Err(_) => return,
                            }
                        };
                        let mut batch: Vec<Msg> = msg.into_iter().collect();
                        let started = Instant::now();
                        while started.elapsed() < FRAME_GAP {
                            match rx.try_recv() {
                                Ok(m) => batch.push(m),
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => return,
                            }
                        }
                        if batch.is_empty() && (pending || how.is_some()) {
                            if let Some(h) = how {
                                if Instant::now() >= until {
                                    end(h);
                                    return;
                                }
                            }
                        }
                        for m in batch {
                            match m {
                                Msg::Bytes(b) => {
                                    engine.feed(&b);
                                    let mut n = in_flight.bytes.lock().unwrap();
                                    *n = n.saturating_sub(b.len());
                                    in_flight.drained.notify_one();
                                }
                                Msg::Key(k) => log(engine.key(&k)),
                                Msg::Text(t) => engine.text(&t),
                                Msg::Paste(p) => log(engine.paste(&p)),
                                Msg::Mouse(m) => log(engine.mouse(m)),
                                Msg::Wheel { lines, x, y, mods } => {
                                    log(engine.wheel(lines, x, y, mods))
                                }
                                Msg::Resize(s) => {
                                    if let Some((restore_at, target)) = nudge_restore {
                                        if target.cols == s.cols && target.rows == s.rows {
                                            // Same logical grid target as the in-flight spaced nudge:
                                            // preserve the 90ms spacing while updating cell pixel metrics.
                                            nudge_restore = Some((restore_at, s));
                                            continue;
                                        }
                                    }
                                    let cur = engine.size();
                                    let grid_changed = cur.cols != s.cols || cur.rows != s.rows;
                                    if grid_changed {
                                        // A genuine grid dimension change already triggers a full
                                        // SIGWINCH / Mode-2048 repaint; cancel any pending same-size nudge.
                                        reattach_nudge_at = None;
                                        nudge_restore = None;
                                        log(engine.resize(s));
                                        let _ = client_clone.resize_session_with_cells(
                                            &id_clone,
                                            s.cols,
                                            s.rows,
                                            u16::try_from(s.cell_w).ok(),
                                            u16::try_from(s.cell_h).ok(),
                                        );
                                    } else {
                                        // Cell pixel metrics (HiDPI scale) may still differ from the
                                        // daemon attach fallback; update pixel size without cancelling
                                        // the same-grid reattach redraw.
                                        if cur != s {
                                            log(engine.resize(s));
                                        }
                                        if reattach_nudge_at.is_some() {
                                            reattach_nudge_at = Some(Instant::now());
                                        }
                                    }
                                }
                                Msg::Colors(c) => log(engine.set_colors(c)),
                                Msg::Clear => log(engine.clear()),
                                Msg::ScrollToRow(row) => {
                                    engine.scroll_to_row(row);
                                }
                                Msg::ScrollToBottom => {
                                    engine.scroll_to_bottom();
                                }
                                Msg::DumpText(resp) => {
                                    let _ = resp.send(engine.dump_plain_text().unwrap_or_default());
                                }
                                Msg::Eof => {
                                    how = Some(Ended::Shell(Exit::Code(0)));
                                    until = Instant::now() + LAST_WORDS;
                                }
                                Msg::Exited(exit) => {
                                    how = Some(Ended::Shell(exit));
                                    until = Instant::now() + LAST_WORDS;
                                }
                                Msg::Shutdown => return,
                            }
                            pending = true;
                        }
                        let out = engine.take_output();
                        if !out.is_empty()
                            && how.is_none()
                            && writer_stream.write_all(&out).is_err()
                        {
                            how = Some(Ended::Broken);
                        }
                        if pending && last.elapsed() >= FRAME_GAP {
                            last = Instant::now();
                            pending = false;
                            match engine.frame() {
                                Ok(Some(frame)) => {
                                    *latest.lock().unwrap() = Some(frame);
                                    wake();
                                }
                                Ok(None) => {}
                                Err(err) => tracing::warn!("terminal frame: {err}"),
                            }
                        }
                    }
                })?;
        }

        Ok(Self {
            inner: Arc::new(SessionInner { tx }),
            latest,
            ended,
            cwd,
            session_id: Some(id),
            remote_computer: None,
            started: Instant::now(),
        })
    }

    /// Connect an interactive terminal session to a remote computer's Gateway
    /// over `GET /api/pty/ws` (using the same E2EE v2 sealing as `/api/ws`).
    pub fn start_remote(
        computer: String,
        gateway: crate::hub::LocalGateway,
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        enum RemoteOut {
            Input(Vec<u8>),
            Resize(Size),
            Close,
        }

        let (tx, rx) = mpsc::channel::<Msg>();
        let (ws_out_tx, mut ws_out_rx) = tokio::sync::mpsc::unbounded_channel::<RemoteOut>();
        let latest = Arc::new(Mutex::new(None));
        let ended = Arc::new(Mutex::new(None));
        let in_flight = Arc::new(InFlight::default());

        // Background tokio thread driving the WebSocket connection to /api/pty/ws.
        {
            let tx = tx.clone();
            let in_flight = in_flight.clone();
            let cwd_str = cwd.to_string_lossy().into_owned();
            std::thread::Builder::new()
                .name("muqun-term-remote".into())
                .spawn(move || {
                    use base64::Engine as _;
                    use futures::{SinkExt as _, StreamExt as _};
                    use serde_json::{json, Value};
                    use tokio_tungstenite::tungstenite::Message;

                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(err) => {
                            let msg = format!("\r\n[Remote terminal runtime error: {err}]\r\n");
                            let _ = tx.send(Msg::Bytes(msg.into_bytes()));
                            let _ = tx.send(Msg::Eof);
                            return;
                        }
                    };
                    rt.block_on(async move {
                        let feed_notice = |text: &str| {
                            let bytes = text.as_bytes().to_vec();
                            *in_flight.bytes.lock().unwrap() += bytes.len();
                            let _ = tx.send(Msg::Bytes(bytes));
                        };
                        let crate::hub::WsUpgradeRequest {
                            request,
                            material,
                            nonce,
                        } = match crate::hub::build_ws_request(
                            &gateway,
                            &["api", "pty", "ws"],
                            "GET /api/pty/ws",
                        ) {
                            Ok(built) => built,
                            Err(err) => {
                                feed_notice(&format!("\r\n[Remote terminal config error: {err}]\r\n"));
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                        };

                        let (mut socket, _) = match tokio::time::timeout(
                            Duration::from_secs(10),
                            tokio_tungstenite::connect_async(request),
                        )
                        .await
                        {
                            Ok(Ok(pair)) => pair,
                            Ok(Err(err)) => {
                                let err_text = err.to_string();
                                if err_text.contains("404") || err_text.contains("501") {
                                    feed_notice(
                                        "\r\n[Remote terminal requires a newer Muqun Gateway on this computer (remote_pty_v1).]\r\n",
                                    );
                                } else {
                                    feed_notice(&format!(
                                        "\r\n[Could not connect to remote terminal: {err_text}]\r\n"
                                    ));
                                }
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                            Err(_) => {
                                feed_notice("\r\n[Remote terminal connection timed out.]\r\n");
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                        };

                        // Read hello (and initialize E2EE codec when transport_key is set).
                        let first_msg = match tokio::time::timeout(
                            Duration::from_secs(10),
                            socket.next(),
                        )
                        .await
                        {
                            Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
                            _ => {
                                feed_notice("\r\n[Remote terminal closed during handshake.]\r\n");
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                        };

                        let mut codec: Option<crate::hub::WsClientCodec> = None;
                        if let (Some(material), Some(request_nonce)) = (material.as_deref(), nonce) {
                            let first_val: Value = match serde_json::from_str(&first_msg) {
                                Ok(v) => v,
                                Err(err) => {
                                    feed_notice(&format!("\r\n[Invalid remote handshake: {err}]\r\n"));
                                    let _ = tx.send(Msg::Eof);
                                    return;
                                }
                            };
                            match crate::hub::WsClientCodec::open_first(
                                &first_val,
                                material,
                                "GET /api/pty/ws",
                                &request_nonce,
                            ) {
                                Ok((c, _hello)) => codec = Some(c),
                                Err(err) => {
                                    feed_notice(&format!(
                                        "\r\n[Remote terminal encryption handshake failed: {err}]\r\n"
                                    ));
                                    let _ = tx.send(Msg::Eof);
                                    return;
                                }
                            }
                        }

                        let ws = winsize(size);
                        let open_plain = json!({
                            "t": "open",
                            "cwd": cwd_str,
                            "cols": ws.cols,
                            "rows": ws.rows,
                            "px_w": ws.px_w,
                            "px_h": ws.px_h,
                        })
                        .to_string();
                        let open_wire = match codec.as_mut() {
                            Some(c) => match c.seal(&open_plain) {
                                Ok(w) => w,
                                Err(_) => {
                                    let _ = tx.send(Msg::Eof);
                                    return;
                                }
                            },
                            None => open_plain,
                        };
                        if socket.send(Message::text(open_wire)).await.is_err() {
                            let _ = tx.send(Msg::Eof);
                            return;
                        }

                        loop {
                            tokio::select! {
                                outbound = ws_out_rx.recv() => {
                                    let Some(cmd) = outbound else {
                                        break;
                                    };
                                    let plain = match cmd {
                                        RemoteOut::Input(bytes) => json!({
                                            "t": "input",
                                            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                                        })
                                        .to_string(),
                                        RemoteOut::Resize(s) => {
                                            let w = winsize(s);
                                            json!({
                                                "t": "resize",
                                                "cols": w.cols,
                                                "rows": w.rows,
                                                "px_w": w.px_w,
                                                "px_h": w.px_h,
                                            })
                                            .to_string()
                                        }
                                        RemoteOut::Close => {
                                            let plain = r#"{"t":"close"}"#.to_string();
                                            let wire = match codec.as_mut() {
                                                Some(c) => c.seal(&plain).unwrap_or(plain),
                                                None => plain,
                                            };
                                            let _ = socket.send(Message::text(wire)).await;
                                            let _ = socket.close(None).await;
                                            return;
                                        }
                                    };
                                    let wire = match codec.as_mut() {
                                        Some(c) => match c.seal(&plain) {
                                            Ok(w) => w,
                                            Err(_) => break,
                                        },
                                        None => plain,
                                    };
                                    if socket.send(Message::text(wire)).await.is_err() {
                                        break;
                                    }
                                }
                                incoming = socket.next() => {
                                    let Some(Ok(msg)) = incoming else {
                                        break;
                                    };
                                    match msg {
                                        Message::Text(text) => {
                                            let plain = match codec.as_mut() {
                                                Some(c) => {
                                                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                                                        break;
                                                    };
                                                    match c.open(&v) {
                                                        Ok(p) => p,
                                                        Err(_) => break,
                                                    }
                                                }
                                                None => text.to_string(),
                                            };
                                            let Ok(val) = serde_json::from_str::<Value>(&plain) else {
                                                continue;
                                            };
                                            match val.get("t").and_then(Value::as_str) {
                                                Some("output") => {
                                                    if let Some(data) = val.get("data").and_then(Value::as_str) {
                                                        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                                                            if !bytes.is_empty() {
                                                                *in_flight.bytes.lock().unwrap() += bytes.len();
                                                                if tx.send(Msg::Bytes(bytes)).is_err() {
                                                                    return;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Some("exit") => {
                                                    let exit = if let Some(sig) = val.get("signal").and_then(Value::as_i64) {
                                                        Exit::Signal(sig as i32)
                                                    } else {
                                                        Exit::Code(val.get("code").and_then(Value::as_i64).unwrap_or(0) as i32)
                                                    };
                                                    let _ = tx.send(Msg::Exited(exit));
                                                    break;
                                                }
                                                Some("error") => {
                                                    let msg = val.get("message").and_then(Value::as_str).unwrap_or("remote error");
                                                    feed_notice(&format!("\r\n[Remote terminal error: {msg}]\r\n"));
                                                    break;
                                                }
                                                _ => {}
                                            }
                                        }
                                        Message::Ping(p) => {
                                            if socket.send(Message::Pong(p)).await.is_err() {
                                                break;
                                            }
                                        }
                                        Message::Close(_) => break,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        let _ = tx.send(Msg::Eof);
                    });
                })?;
        }

        // Emulator thread (same libghostty-vt loop as local sessions).
        {
            let latest = latest.clone();
            let ended = ended.clone();
            std::thread::Builder::new()
                .name("muqun-term".into())
                .spawn(move || {
                    let _release = Release(in_flight.clone());
                    let end = |how: Ended| {
                        *ended.lock().unwrap() = Some(how);
                        wake();
                    };
                    let mut engine = match Engine::new(size, colors) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::warn!("terminal: could not start the emulator: {err}");
                            end(Ended::Broken);
                            return;
                        }
                    };
                    let mut last = Instant::now() - FRAME_GAP;
                    let mut pending = false;
                    let mut how: Option<Ended> = None;
                    let mut eof = false;
                    let mut until = Instant::now();
                    loop {
                        if let Some(how) = how.filter(|_| eof || Instant::now() >= until) {
                            if pending {
                                if let Ok(Some(frame)) = engine.frame() {
                                    *latest.lock().unwrap() = Some(frame);
                                }
                            }
                            end(how);
                            return;
                        }
                        let msg = if pending || how.is_some() {
                            let mut wait =
                                (last + FRAME_GAP).saturating_duration_since(Instant::now());
                            if how.is_some() {
                                wait = until.saturating_duration_since(Instant::now());
                                if pending {
                                    wait = wait.min(
                                        (last + FRAME_GAP)
                                            .saturating_duration_since(Instant::now()),
                                    );
                                }
                            }
                            match rx.recv_timeout(wait) {
                                Ok(m) => Some(m),
                                Err(RecvTimeoutError::Timeout) => None,
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        } else {
                            match rx.recv() {
                                Ok(m) => Some(m),
                                Err(_) => return,
                            }
                        };
                        let mut batch: Vec<Msg> = msg.into_iter().collect();
                        let started = Instant::now();
                        while started.elapsed() < FRAME_GAP {
                            match rx.try_recv() {
                                Ok(m) => batch.push(m),
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => return,
                            }
                        }
                        for m in batch {
                            match m {
                                Msg::Bytes(b) => {
                                    engine.feed(&b);
                                    let mut n = in_flight.bytes.lock().unwrap();
                                    *n = n.saturating_sub(b.len());
                                    in_flight.drained.notify_one();
                                }
                                Msg::Key(k) => log(engine.key(&k)),
                                Msg::Text(t) => engine.text(&t),
                                Msg::Paste(t) => log(engine.paste(&t)),
                                Msg::Mouse(m) => log(engine.mouse(m)),
                                Msg::Wheel { lines, x, y, mods } => {
                                    log(engine.wheel(lines, x, y, mods))
                                }
                                Msg::Resize(s) => {
                                    if engine.size() != s {
                                        log(engine.resize(s));
                                        let _ = ws_out_tx.send(RemoteOut::Resize(s));
                                    }
                                }
                                Msg::Colors(c) => log(engine.set_colors(c)),
                                Msg::Clear => log(engine.clear()),
                                Msg::ScrollToRow(row) => engine.scroll_to_row(row),
                                Msg::ScrollToBottom => engine.scroll_to_bottom(),
                                Msg::DumpText(resp) => {
                                    let _ = resp.send(engine.dump_plain_text().unwrap_or_default());
                                }
                                Msg::Eof => {
                                    eof = true;
                                    if how.is_none() {
                                        how = Some(Ended::Shell(Exit::Code(0)));
                                        until = Instant::now() + LAST_WORDS;
                                    }
                                }
                                Msg::Exited(exit) => {
                                    how = Some(Ended::Shell(exit));
                                    until = Instant::now() + LAST_WORDS;
                                }
                                Msg::Shutdown => {
                                    let _ = ws_out_tx.send(RemoteOut::Close);
                                    return;
                                }
                            }
                            pending = true;
                        }
                        let out = engine.take_output();
                        if !out.is_empty()
                            && how.is_none()
                            && ws_out_tx.send(RemoteOut::Input(out)).is_err()
                        {
                            how = Some(Ended::Broken);
                        }
                        if pending && last.elapsed() >= FRAME_GAP {
                            last = Instant::now();
                            pending = false;
                            match engine.frame() {
                                Ok(Some(frame)) => {
                                    *latest.lock().unwrap() = Some(frame);
                                    wake();
                                }
                                Ok(None) => {}
                                Err(err) => tracing::warn!("terminal frame: {err}"),
                            }
                        }
                    }
                })?;
        }

        Ok(Self {
            inner: Arc::new(SessionInner { tx }),
            latest,
            ended,
            cwd,
            session_id: None,
            remote_computer: Some(computer),
            started: Instant::now(),
        })
    }

    fn start_on(
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
        spawn: impl FnOnce(
            &std::path::Path,
            WinSize,
            pty::OnExit,
        ) -> std::io::Result<(pty::Pty, pty::Child)>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let on_exit: pty::OnExit = {
            let tx = tx.clone();
            Box::new(move |exit| {
                let _ = tx.send(Msg::Exited(exit));
            })
        };
        let (mut pty, child) = spawn(&cwd, winsize(size), on_exit)?;
        let mut reader = pty.reader()?;
        let latest = Arc::new(Mutex::new(None));
        let ended = Arc::new(Mutex::new(None));
        let in_flight = Arc::new(InFlight::default());

        {
            let tx = tx.clone();
            let in_flight = in_flight.clone();
            std::thread::Builder::new()
                .name("muqun-term-read".into())
                .spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        {
                            let mut n = in_flight.bytes.lock().unwrap();
                            while *n > MAX_IN_FLIGHT {
                                n = in_flight.drained.wait(n).unwrap();
                            }
                        }
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => {
                                let _ = tx.send(Msg::Eof);
                                return;
                            }
                            Ok(n) => {
                                *in_flight.bytes.lock().unwrap() += n;
                                if tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                })?;
        }

        {
            let latest = latest.clone();
            let ended = ended.clone();
            std::thread::Builder::new()
                .name("muqun-term".into())
                .spawn(move || {
                    // Dropped with the thread: hangs up a shell still
                    // running, and stops the reader.
                    let _child = child;
                    let _release = Release(in_flight.clone());
                    let end = |how: Ended| {
                        *ended.lock().unwrap() = Some(how);
                        wake();
                    };
                    let mut engine = match Engine::new(size, colors) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::warn!("terminal: could not start the emulator: {err}");
                            end(Ended::Broken);
                            return;
                        }
                    };
                    let mut last = Instant::now() - FRAME_GAP;
                    let mut pending = false;
                    // How it ended, once it has; and whether all its output
                    // is in, or until when it may still come.
                    let mut how: Option<Ended> = None;
                    let mut eof = false;
                    let mut until = Instant::now();
                    loop {
                        if let Some(how) = how.filter(|_| eof || Instant::now() >= until) {
                            // The last frame, then the news.
                            if pending {
                                if let Ok(Some(frame)) = engine.frame() {
                                    *latest.lock().unwrap() = Some(frame);
                                }
                            }
                            end(how);
                            return;
                        }
                        let msg = if pending || how.is_some() {
                            let mut wait =
                                (last + FRAME_GAP).saturating_duration_since(Instant::now());
                            if how.is_some() {
                                wait = until.saturating_duration_since(Instant::now());
                                if pending {
                                    wait = wait.min(
                                        (last + FRAME_GAP)
                                            .saturating_duration_since(Instant::now()),
                                    );
                                }
                            }
                            match rx.recv_timeout(wait) {
                                Ok(m) => Some(m),
                                Err(RecvTimeoutError::Timeout) => None,
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        } else {
                            match rx.recv() {
                                Ok(m) => Some(m),
                                Err(_) => return,
                            }
                        };
                        // Fold everything already queued into this frame.
                        let mut batch: Vec<Msg> = msg.into_iter().collect();
                        let started = Instant::now();
                        while started.elapsed() < FRAME_GAP {
                            match rx.try_recv() {
                                Ok(m) => batch.push(m),
                                Err(mpsc::TryRecvError::Empty) => break,
                                Err(mpsc::TryRecvError::Disconnected) => return,
                            }
                        }
                        for m in batch {
                            match m {
                                Msg::Bytes(b) => {
                                    engine.feed(&b);
                                    let mut n = in_flight.bytes.lock().unwrap();
                                    *n = n.saturating_sub(b.len());
                                    in_flight.drained.notify_one();
                                }
                                Msg::Key(k) => log(engine.key(&k)),
                                Msg::Text(t) => engine.text(&t),
                                Msg::Paste(t) => log(engine.paste(&t)),
                                Msg::Mouse(m) => log(engine.mouse(m)),
                                Msg::Wheel { lines, x, y, mods } => {
                                    log(engine.wheel(lines, x, y, mods))
                                }
                                Msg::Resize(s) => {
                                    if engine.size() != s {
                                        log(engine.resize(s));
                                        pty.resize(winsize(s));
                                    }
                                }
                                Msg::Colors(c) => log(engine.set_colors(c)),
                                Msg::Clear => log(engine.clear()),
                                Msg::ScrollToRow(row) => {
                                    engine.scroll_to_row(row);
                                }
                                Msg::ScrollToBottom => {
                                    engine.scroll_to_bottom();
                                }
                                Msg::DumpText(resp) => {
                                    let _ = resp.send(engine.dump_plain_text().unwrap_or_default());
                                }
                                Msg::Eof => eof = true,
                                Msg::Exited(exit) => {
                                    how = Some(Ended::Shell(exit));
                                    until = Instant::now() + LAST_WORDS;
                                }
                                Msg::Shutdown => return,
                            }
                            pending = true;
                        }
                        let out = engine.take_output();
                        if !out.is_empty() && how.is_none() && pty.write_all(&out).is_err() {
                            how = Some(Ended::Broken);
                        }
                        if pending && last.elapsed() >= FRAME_GAP {
                            last = Instant::now();
                            pending = false;
                            match engine.frame() {
                                Ok(Some(frame)) => {
                                    *latest.lock().unwrap() = Some(frame);
                                    wake();
                                }
                                Ok(None) => {}
                                Err(err) => tracing::warn!("terminal frame: {err}"),
                            }
                        }
                    }
                })?;
        }
        Ok(Self {
            inner: Arc::new(SessionInner { tx }),
            latest,
            ended,
            cwd,
            session_id: None,
            remote_computer: None,
            started: Instant::now(),
        })
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.inner.tx.send(msg);
    }

    /// Scroll the emulator viewport to row `row`.
    pub fn scroll_to_row(&self, row: usize) {
        self.send(Msg::ScrollToRow(row));
    }

    /// Scroll the emulator viewport to the bottom.
    pub fn scroll_to_bottom(&self) {
        self.send(Msg::ScrollToBottom);
    }

    /// Request a dump of the plain text buffer across scrollback and screen.
    pub fn dump_plain_text(&self) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Msg::DumpText(tx));
        rx.recv_timeout(Duration::from_millis(200)).ok()
    }

    /// Explicitly closes the local daemon session if backed by one.
    pub fn close_remote(&self) {
        if self.remote_computer.is_some() {
            self.send(Msg::Shutdown);
            return;
        }
        #[cfg(unix)]
        if let Some(id) = &self.session_id {
            let client = super::daemon::SyncDaemonClient::new(None);
            let _ = client.close_session(id);
        }
    }

    /// The newest frame since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// How the session ended, once it has (its last frame is published
    /// before).
    pub fn ended(&self) -> Option<Ended> {
        *self.ended.lock().unwrap()
    }
}

fn winsize(s: Size) -> WinSize {
    WinSize {
        cols: s.cols,
        rows: s.rows,
        px_w: (u32::from(s.cols) * s.cell_w).min(u32::from(u16::MAX)) as u16,
        px_h: (u32::from(s.rows) * s.cell_h).min(u32::from(u16::MAX)) as u16,
    }
}

fn log(r: Result<(), libghostty_vt::Error>) {
    if let Err(err) = r {
        tracing::debug!("terminal: {err}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::result_large_err)]
    fn start_remote_e2ee_v2_roundtrip_drives_emulator_and_resize() {
        use base64::Engine as _;
        use futures::{SinkExt as _, StreamExt as _};
        use muqun_gateway::connectivity::transport;
        use serde_json::{json, Value};
        use tokio_tungstenite::tungstenite::Message;

        let material = [0x5au8; 32];
        let transport_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(material);
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (seen_resize_tx, seen_resize_rx) = std::sync::mpsc::channel();

        // Spawn an E2EE v2 /api/pty/ws peer verifying Desktop's exact upgrade headers,
        // per-frame AAD, open/input/resize frames, and exit handling.
        let server = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let (tcp, _) = listener.accept().await.unwrap();

                let mut captured_nonce = None;
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     mut resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        assert_eq!(req.uri().path(), "/api/pty/ws");
                        assert_eq!(
                            req.headers().get("x-muqun-transport").and_then(|v| v.to_str().ok()),
                            Some("1")
                        );
                        assert_eq!(
                            req.headers().get("x-muqun-device").and_then(|v| v.to_str().ok()),
                            Some("dev-desktop-test")
                        );
                        let env_b64 = req
                            .headers()
                            .get("x-muqun-envelope")
                            .and_then(|v| v.to_str().ok())
                            .unwrap();
                        let env_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(env_b64)
                            .unwrap();
                        let env: transport::Envelope = serde_json::from_slice(&env_bytes).unwrap();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap()
                            .as_millis();
                        let pt = transport::open(
                            &material,
                            transport::Direction::Request,
                            b"GET /api/pty/ws",
                            &env,
                            now_ms,
                        )
                        .unwrap();
                        let payload: Value = serde_json::from_slice(&pt).unwrap();
                        assert_eq!(payload["token"], "remote-secret-token");
                        captured_nonce = Some(env.nonce);
                        resp.headers_mut()
                            .insert("x-muqun-transport", "1".parse().unwrap());
                        Ok(resp)
                    },
                )
                .await
                .unwrap();

                let nonce = captured_nonce.unwrap();
                let cid = "conn-desktop-pty-1";
                let server_key = transport::derive_ws_server_key(&material, cid, &nonce).unwrap();
                let client_key = transport::derive_ws_client_key(&material, cid, &nonce).unwrap();
                let mut send_seq = 0u64;
                let mut recv_seq = 0u64;

                let seal = |plain: &str, send_seq: &mut u64| -> String {
                    let s = *send_seq;
                    let aad = format!("GET /api/pty/ws\n{cid}\n{s}");
                    let c = transport::seal_stream_event(
                        &server_key,
                        s,
                        aad.as_bytes(),
                        plain.as_bytes(),
                    )
                    .unwrap();
                    *send_seq += 1;
                    if s == 0 {
                        json!({"cid": cid, "seq": s, "c": c}).to_string()
                    } else {
                        json!({"seq": s, "c": c}).to_string()
                    }
                };
                let open = |wire: &str, recv_seq: &mut u64| -> Value {
                    let v: Value = serde_json::from_str(wire).unwrap();
                    let s = *recv_seq;
                    assert_eq!(v["seq"].as_u64(), Some(s));
                    let aad = format!("GET /api/pty/ws\n{cid}\n{s}");
                    let pt = transport::open_stream_event(
                        &client_key,
                        s,
                        aad.as_bytes(),
                        v["c"].as_str().unwrap(),
                    )
                    .unwrap();
                    *recv_seq += 1;
                    serde_json::from_slice(&pt).unwrap()
                };

                // 1. Send hello
                socket
                    .send(Message::text(seal(
                        &json!({"t": "hello", "connection_id": cid, "protocol": 1}).to_string(),
                        &mut send_seq,
                    )))
                    .await
                    .unwrap();

                // 2. Receive open
                let open_msg = socket.next().await.unwrap().unwrap();
                let open_val = open(open_msg.to_text().unwrap(), &mut recv_seq);
                assert_eq!(open_val["t"], "open");
                assert_eq!(open_val["cwd"], "/srv/project");

                // 3. Send opened + prompt output
                socket
                    .send(Message::text(seal(
                        &json!({"t": "opened", "cwd": "/srv/project", "cols": 80, "rows": 24})
                            .to_string(),
                        &mut send_seq,
                    )))
                    .await
                    .unwrap();
                socket
                    .send(Message::text(seal(
                        &json!({
                            "t": "output",
                            "data": base64::engine::general_purpose::STANDARD.encode(b"REMOTE_READY$ "),
                        })
                        .to_string(),
                        &mut send_seq,
                    )))
                    .await
                    .unwrap();

                // 4. Read resize + input, echo back, then exit 0
                let mut saw_input = false;
                let mut saw_resize = false;
                while let Some(Ok(Message::Text(wire))) = socket.next().await {
                    let msg = open(&wire, &mut recv_seq);
                    match msg["t"].as_str() {
                        Some("resize") => {
                            saw_resize = msg["cols"] == 100 && msg["rows"] == 32;
                            let _ = seen_resize_tx.send(saw_resize);
                        }
                        Some("input") => {
                            let raw = base64::engine::general_purpose::STANDARD
                                .decode(msg["data"].as_str().unwrap())
                                .unwrap();
                            let text = String::from_utf8_lossy(&raw);
                            if text.contains("nvim '/srv/project/main.rs'") {
                                saw_input = true;
                                socket
                                    .send(Message::text(seal(
                                        &json!({
                                            "t": "output",
                                            "data": base64::engine::general_purpose::STANDARD
                                                .encode(b"NVIM_OPENED_OK\r\n"),
                                        })
                                        .to_string(),
                                        &mut send_seq,
                                    )))
                                    .await
                                    .unwrap();
                            }
                        }
                        _ => {}
                    }
                    if saw_input && saw_resize {
                        socket
                            .send(Message::text(seal(
                                &json!({"t": "exit", "code": 0}).to_string(),
                                &mut send_seq,
                            )))
                            .await
                            .unwrap();
                        break;
                    }
                }
            });
        });

        let addr = addr_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let gateway = crate::hub::LocalGateway {
            url: format!("http://{addr}"),
            token: "remote-secret-token".into(),
            device_id: Some("dev-desktop-test".into()),
            transport_key: Some(transport_key),
        };
        let session = Session::start_remote(
            "studio-mac".into(),
            gateway,
            PathBuf::from("/srv/project"),
            Size {
                cols: 80,
                rows: 24,
                cell_w: 8,
                cell_h: 17,
            },
            Colors {
                fg: crate::term::grid::Rgb(255, 255, 255),
                bg: crate::term::grid::Rgb(0, 0, 0),
                cursor: crate::term::grid::Rgb(255, 255, 255),
                selection: crate::term::grid::Rgb(64, 64, 64),
                ansi: [crate::term::grid::Rgb(128, 128, 128); 16],
            },
            Arc::new(|| {}),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if session
                .dump_plain_text()
                .is_some_and(|t| t.contains("REMOTE_READY$"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            session
                .dump_plain_text()
                .unwrap_or_default()
                .contains("REMOTE_READY$"),
            "remote prompt never reached libghostty-vt emulator"
        );

        session.send(Msg::Resize(Size {
            cols: 100,
            rows: 32,
            cell_w: 8,
            cell_h: 17,
        }));
        session.send(Msg::Text("nvim '/srv/project/main.rs'\n".into()));

        assert!(
            seen_resize_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or(false),
            "remote server did not receive resized dimensions"
        );
        while Instant::now() < deadline {
            if session.ended().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(session.ended(), Some(Ended::Shell(Exit::Code(0))));
        let frame = session.take_frame().expect("final remote frame published");
        let rendered: String = frame
            .grid
            .iter()
            .map(|row| row.plain_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("NVIM_OPENED_OK"),
            "expected NVIM_OPENED_OK in final rendered frame, got: {rendered:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn wrapped_attach_snapshot_restores_mode_2048_in_band_resize_on_engine() {
        use super::super::daemon::ScrollbackBuffer;
        use super::super::grid::Rgb;

        // Simulate a long-running Neovim 0.10+ session whose 1 MiB daemon ring
        // has wrapped and evicted the startup `\x1b[?1049h\x1b[?2048h` sequences.
        let mut sb = ScrollbackBuffer::new(128);
        sb.push(b"\x1b[?1049h\x1b[?2048h");
        sb.push(&[b' '; 256]);
        sb.push(b"\x1b[1;1HNVIM_FRAME");

        let colors = Colors {
            fg: Rgb(240, 240, 240),
            bg: Rgb(18, 18, 18),
            cursor: Rgb(255, 255, 255),
            selection: Rgb(60, 80, 120),
            ansi: [Rgb(128, 128, 128); 16],
        };
        let mut engine = Engine::new(
            Size {
                cols: 132,
                rows: 73,
                cell_w: 8,
                cell_h: 17,
            },
            colors,
        )
        .unwrap();

        // Reattach replay: feed wrapped snapshot, then discard historical replies.
        engine.feed(&sb.to_attach_vec());
        let _ = engine.take_output();

        // Resizing the reattached pane MUST emit CSI 48;<rows>;<cols>;...t
        // in-band resize report so Mode-2048 TUIs (Neovim) repaint at 110x34.
        engine
            .resize(Size {
                cols: 110,
                rows: 34,
                cell_w: 8,
                cell_h: 17,
            })
            .unwrap();
        let report = String::from_utf8(engine.take_output()).unwrap();
        assert!(
            report.contains("\x1b[48;34;110;"),
            "expected Mode 2048 in-band resize report after wrapped reattach, got: {report:?}"
        );
    }
}
