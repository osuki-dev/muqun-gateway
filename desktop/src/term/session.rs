//! One live terminal: the shell in a pty, a reader thread, and the emulator
//! thread that owns the [`Engine`] (libghostty types never leave it).
//!
//! The UI talks to it with [`Msg`]s and takes frames from a latest-wins slot;
//! the emulator wakes the window when it publishes one, at most every
//! [`FRAME_GAP`], so a flood (`yes`, `cat` of a big file) costs the UI one
//! frame per display refresh however fast the shell writes.

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::engine::{Colors, Engine, MouseInput, Size};
use super::grid::Frame;
use super::input::{KeyInput, Mods};
use crate::platform::pty::{self, WinSize};

/// The shortest time between two published frames.
const FRAME_GAP: Duration = Duration::from_millis(8);
/// Bytes read from the shell but not yet parsed, at most: past this the
/// reader stops reading and the shell blocks on its writes.
const MAX_IN_FLIGHT: usize = 4 << 20;

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
    Eof,
    /// The panel closed: hang the shell up and stop.
    Shutdown,
}

/// Bytes between the reader and the emulator, for back-pressure.
#[derive(Default)]
struct InFlight {
    bytes: Mutex<usize>,
    drained: Condvar,
}

pub struct Session {
    tx: mpsc::Sender<Msg>,
    latest: Arc<Mutex<Option<Frame>>>,
    exited: Arc<AtomicBool>,
    pub cwd: PathBuf,
}

impl Session {
    /// Start the owner's shell in `cwd`; `wake` asks the window for a frame.
    pub fn start(
        cwd: PathBuf,
        size: Size,
        colors: Colors,
        wake: rune::Waker,
    ) -> std::io::Result<Self> {
        let (mut pty, child) = pty::spawn_shell(&cwd, winsize(size))?;
        let mut reader = pty.reader()?;
        let (tx, rx) = mpsc::channel::<Msg>();
        let latest = Arc::new(Mutex::new(None));
        let exited = Arc::new(AtomicBool::new(false));
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
            let exited = exited.clone();
            std::thread::Builder::new()
                .name("muqun-term".into())
                .spawn(move || {
                    // Dropped with the thread: hangs the shell up.
                    let _child = child;
                    let mut engine = match Engine::new(size, colors) {
                        Ok(e) => e,
                        Err(err) => {
                            tracing::warn!("terminal: could not start the emulator: {err}");
                            exited.store(true, Ordering::Release);
                            wake();
                            return;
                        }
                    };
                    let mut last = Instant::now() - FRAME_GAP;
                    let mut pending = false;
                    loop {
                        let msg = if pending {
                            let wait = (last + FRAME_GAP).saturating_duration_since(Instant::now());
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
                                Msg::Eof => {
                                    exited.store(true, Ordering::Release);
                                }
                                Msg::Shutdown => return,
                            }
                            pending = true;
                        }
                        let out = engine.take_output();
                        if !out.is_empty() && pty.write_all(&out).is_err() {
                            exited.store(true, Ordering::Release);
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
                            if exited.load(Ordering::Acquire) {
                                wake();
                                return;
                            }
                        }
                    }
                })?;
        }
        Ok(Self {
            tx,
            latest,
            exited,
            cwd,
        })
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// The newest frame since the last call.
    pub fn take_frame(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// The shell ended (or never started).
    pub fn exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
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
