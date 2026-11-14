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
    /// When the shell started.
    pub started: Instant,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("cwd", &self.cwd)
            .field("session_id", &self.session_id)
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
            None => client.create_session(None, Some(cwd.clone()), size.cols, size.rows)?,
        };
        let mut stream = client.attach_raw(&id)?;
        let mut initial_scrollback = Vec::new();
        if is_reattach {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(25)));
            let mut sb_buf = [0u8; 64 * 1024];
            loop {
                match stream.read(&mut sb_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        initial_scrollback.extend_from_slice(&sb_buf[..n]);
                        if initial_scrollback.len() >= 1024 * 1024 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = stream.set_read_timeout(None);
            let _ = client.resize_session(&id, size.cols, size.rows);
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
                        engine.feed(&initial_scrollback);
                        // Discard historical terminal query replies (DECRQM, DA1, DSR, Kitty)
                        // so reattaching never injects escape sequences into the live PTY stdin.
                        let _ = engine.take_output();
                        if let Ok(Some(frame)) = engine.frame() {
                            *latest.lock().unwrap() = Some(frame);
                            wake();
                        }
                    }
                    let mut last = Instant::now() - FRAME_GAP;
                    let mut pending = false;
                    let mut how: Option<Ended> = None;
                    let mut until = Instant::now();

                    loop {
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
                                    if engine.size() != s {
                                        log(engine.resize(s));
                                        let _ =
                                            client_clone.resize_session(&id_clone, s.cols, s.rows);
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

    /// Explicitly closes the remote daemon session if backed by one.
    pub fn close_remote(&self) {
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
