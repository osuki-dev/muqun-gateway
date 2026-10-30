//! Process host for `pi --mode rpc`: one child per live Pi session.
//!
//! Pi's RPC mode is strict JSONL on stdin/stdout (`docs/rpc.md` in the Pi
//! install). Records are split on LF only -- a generic line reader that also
//! breaks on U+2028/U+2029 would cut JSON strings apart -- and an optional CR
//! before the LF is dropped. Commands carry a string `id` that the matching
//! `response` repeats; everything else on stdout is a session or extension-UI
//! event and is fanned out to subscribers untouched. Stderr is diagnostics
//! only and is never parsed.
//!
//! The host only holds children; the driver decides when to start one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, watch, Notify};

use crate::agents::ports::agent::AgentError;

/// A record this long is not a protocol message we can use: holding it would
/// let one runaway tool output pin unbounded memory, and resynchronising
/// mid-stream is guesswork, so the child is treated as broken.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// Stderr lines are only logged; a longer one is dropped in pieces, not buffered.
const MAX_STDERR_LINE_BYTES: usize = 64 * 1024;
/// Pi answers every command once it is handled (a `prompt` is answered on
/// acceptance, not completion), so a minute without one means it is stuck.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Closing stdin asks Pi to dispose its runtime; it gets this long to do so.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
/// After the process exits, stdout may still hold responses already written;
/// pending requests are failed only once the reader drained it (or this ran out,
/// for a grandchild that inherited stdout and keeps it open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// Text deltas arrive in bursts; a slow subscriber lags rather than blocking
/// the reader (which would stall Pi through stdout backpressure).
const EVENT_CAPACITY: usize = 4096;

/// Where the `pi` binary and the bundled muqun extension live.
#[derive(Debug, Clone)]
pub struct PiCommandLine {
    pub binary: PathBuf,
    pub extension: PathBuf,
}

/// What one child is started for.
#[derive(Debug, Clone)]
pub struct PiLaunch {
    /// Resume this session file; `None` lets Pi create a new session in `cwd`.
    pub session_file: Option<PathBuf>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// A throwaway probe (model catalog): `--no-extensions` and no muqun
    /// extension, so user extensions cannot slow or break it; without a
    /// session file it also passes `--no-session` so no session is written.
    pub no_extensions: bool,
}

impl PiLaunch {
    fn args(&self, cmd: &PiCommandLine) -> Vec<std::ffi::OsString> {
        let mut args: Vec<std::ffi::OsString> = vec!["--mode".into(), "rpc".into()];
        if let Some(file) = &self.session_file {
            args.push("--session".into());
            args.push(file.into());
        } else if self.no_extensions {
            args.push("--no-session".into());
        }
        if self.no_extensions {
            args.push("--no-extensions".into());
        } else {
            args.push("-e".into());
            args.push(cmd.extension.clone().into());
        }
        args
    }
}

type Reply = Result<Value, AgentError>;

#[derive(Default)]
struct Pending {
    waiters: HashMap<String, oneshot::Sender<Reply>>,
    /// Set once no response can arrive any more; later requests fail fast
    /// instead of waiting out the timeout.
    closed: Option<AgentError>,
}

#[derive(Default)]
struct Shared {
    pending: Mutex<Pending>,
}

impl Shared {
    fn fail_all(&self, error: AgentError) {
        let waiters = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            if pending.closed.is_none() {
                pending.closed = Some(error.clone());
            }
            std::mem::take(&mut pending.waiters)
        };
        for (_, waiter) in waiters {
            let _ = waiter.send(Err(error.clone()));
        }
    }

    fn resolve(&self, id: &str, reply: Reply) -> bool {
        let waiter = match self.pending.lock() {
            Ok(mut pending) => pending.waiters.remove(id),
            Err(_) => None,
        };
        match waiter {
            Some(waiter) => {
                let _ = waiter.send(reply);
                true
            }
            None => false,
        }
    }
}

/// Removes a pending entry when a request is cancelled or times out, so the
/// map does not grow with waiters nobody listens to.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() {
            pending.waiters.remove(&self.id);
        }
    }
}

/// One `pi --mode rpc` process.
pub struct PiChild {
    pid: Option<u32>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    events: broadcast::Sender<Value>,
    alive: Arc<AtomicBool>,
    last_used: Arc<Mutex<Instant>>,
    kill: Arc<Notify>,
    exited: watch::Receiver<bool>,
}

impl PiChild {
    pub async fn spawn(cmd: &PiCommandLine, launch: &PiLaunch) -> Result<Self, AgentError> {
        let mut child = Command::new(&cmd.binary)
            .args(launch.args(cmd))
            .current_dir(&launch.cwd)
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A gateway that dies or drops the host must not leave Pi behind.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                AgentError::NotAvailable(format!("cannot start {}: {e}", cmd.binary.display()))
            })?;
        let pid = child.id();
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (i, o, e),
                _ => return Err(AgentError::NotAvailable("pi pipes unavailable".into())),
            };

        let shared = Arc::new(Shared::default());
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let last_used = Arc::new(Mutex::new(Instant::now()));
        let kill = Arc::new(Notify::new());
        let (exited_tx, exited) = watch::channel(false);

        let reader = tokio::spawn(read_stdout(
            BufReader::new(stdout),
            pid,
            shared.clone(),
            events.clone(),
            last_used.clone(),
            alive.clone(),
            kill.clone(),
        ));
        tokio::spawn(log_stderr(BufReader::new(stderr), pid));
        {
            let shared = shared.clone();
            let alive = alive.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let status = tokio::select! {
                    status = child.wait() => status,
                    _ = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };
                alive.store(false, Ordering::SeqCst);
                tracing::debug!(?pid, ?status, "pi exited");
                let _ = tokio::time::timeout(DRAIN_GRACE, reader).await;
                shared.fail_all(AgentError::NotAvailable("pi exited".into()));
                let _ = exited_tx.send(true);
            });
        }

        Ok(Self {
            pid,
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            shared,
            next_id: AtomicU64::new(1),
            events,
            alive,
            last_used,
            kill,
            exited,
        })
    }

    /// Send a command, await its `response` (matched by `id`), 60 s timeout.
    /// `Ok(data)` (`Null` when the response has none) or `RequestFailed(error)`.
    pub async fn request(&self, mut command: Value) -> Result<Value, AgentError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let Some(object) = command.as_object_mut() else {
            return Err(AgentError::InvalidRequest(
                "pi command must be an object".into(),
            ));
        };
        object.insert("id".into(), Value::String(id.clone()));

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self
                .shared
                .pending
                .lock()
                .map_err(|_| AgentError::NotAvailable("pi host poisoned".into()))?;
            if let Some(error) = &pending.closed {
                return Err(error.clone());
            }
            pending.waiters.insert(id.clone(), tx);
        }
        let _guard = PendingGuard {
            shared: &self.shared,
            id,
        };

        let exchange = async {
            self.write(&command).await?;
            rx.await
                .unwrap_or_else(|_| Err(AgentError::NotAvailable("pi exited".into())))
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, exchange).await {
            Ok(reply) => reply,
            Err(_) => {
                tracing::warn!(pid = ?self.pid, command = ?command.get("type"), "pi request timed out");
                Err(AgentError::Network("pi request timed out".into()))
            }
        }
    }

    /// Fire a record with no response (`extension_ui_response`).
    pub async fn send(&self, record: Value) -> Result<(), AgentError> {
        self.write(&record).await
    }

    /// Every non-response stdout record. Subscribe before sending a `prompt`,
    /// or a fast run can settle before the subscriber exists.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Commands and stdout records touch on their own; the driver touches
    /// for activity the child cannot see.
    pub fn touch(&self) {
        touch(&self.last_used);
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used
            .lock()
            .map(|at| at.elapsed())
            .unwrap_or_default()
    }

    /// Close stdin so Pi shuts down in order, wait up to 5 s, then kill.
    pub async fn close(&self) {
        self.stdin.lock().await.take();
        let mut exited = self.exited.clone();
        if tokio::time::timeout(CLOSE_GRACE, exited.wait_for(|done| *done))
            .await
            .is_err()
        {
            tracing::debug!(pid = ?self.pid, "pi ignored stdin close; killing");
            self.kill.notify_one();
            let _ = exited.wait_for(|done| *done).await;
        }
    }

    async fn write(&self, record: &Value) -> Result<(), AgentError> {
        let mut line = serde_json::to_vec(record)
            .map_err(|e| AgentError::InvalidRequest(format!("pi record: {e}")))?;
        line.push(b'\n');
        self.touch();
        let mut stdin = self.stdin.lock().await;
        let Some(pipe) = stdin.as_mut() else {
            return Err(AgentError::NotAvailable("pi stdin closed".into()));
        };
        let written = async {
            pipe.write_all(&line).await?;
            pipe.flush().await
        };
        written
            .await
            .map_err(|e| AgentError::NotAvailable(format!("pi stdin: {e}")))
    }
}

fn touch(last_used: &Mutex<Instant>) {
    if let Ok(mut at) = last_used.lock() {
        *at = Instant::now();
    }
}

enum Line {
    Record,
    TooLong,
    Eof,
}

/// Read up to the next LF into `line` (LF excluded). Stops with `TooLong`
/// as soon as the line passes `max`, without buffering the rest; an
/// unterminated tail at EOF still counts as a record.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                Line::Eof
            } else {
                Line::Record
            });
        }
        let (chunk, used, done) = match available.iter().position(|b| *b == b'\n') {
            Some(at) => (&available[..at], at + 1, true),
            None => (available, available.len(), false),
        };
        if line.len() + chunk.len() > max {
            return Ok(Line::TooLong);
        }
        line.extend_from_slice(chunk);
        reader.consume(used);
        if done {
            return Ok(Line::Record);
        }
    }
}

async fn read_stdout<R: AsyncBufRead + Unpin>(
    mut reader: R,
    pid: Option<u32>,
    shared: Arc<Shared>,
    events: broadcast::Sender<Value>,
    last_used: Arc<Mutex<Instant>>,
    alive: Arc<AtomicBool>,
    kill: Arc<Notify>,
) {
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line, MAX_LINE_BYTES).await {
            Ok(Line::Record) => {}
            Ok(Line::Eof) => break,
            Ok(Line::TooLong) => {
                tracing::warn!(
                    ?pid,
                    limit_bytes = MAX_LINE_BYTES,
                    "pi stdout record too long; killing"
                );
                alive.store(false, Ordering::SeqCst);
                shared.fail_all(AgentError::Protocol(format!(
                    "pi stdout record exceeds {MAX_LINE_BYTES} bytes"
                )));
                kill.notify_one();
                return;
            }
            Err(error) => {
                tracing::warn!(?pid, %error, "pi stdout read failed");
                break;
            }
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        touch(&last_used);
        let record = match serde_json::from_slice::<Value>(&line) {
            Ok(record @ Value::Object(_)) => record,
            Ok(_) => {
                tracing::warn!(
                    ?pid,
                    record_bytes = line.len(),
                    "pi stdout record is not an object"
                );
                continue;
            }
            Err(error) => {
                tracing::warn!(?pid, record_bytes = line.len(), %error, "pi stdout record is not JSON");
                continue;
            }
        };
        if record.get("type").and_then(Value::as_str) != Some("response") {
            let _ = events.send(record);
            continue;
        }
        let command = record.get("command").and_then(Value::as_str).unwrap_or("");
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            // A parse failure on our own output, or a command sent without
            // an id: nothing can be waiting on it.
            tracing::warn!(?pid, command, error = ?record.get("error"), "pi response without id");
            continue;
        };
        let reply = if record.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(record.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Err(AgentError::RequestFailed(
                record
                    .get("error")
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("pi {command} failed"), str::to_owned),
            ))
        };
        if !shared.resolve(id, reply) {
            tracing::warn!(?pid, id, command, "pi response for unknown id");
        }
    }
    // Without stdout the child can answer nothing: callers that see the
    // failure must also see it dead, so they respawn rather than retry it.
    alive.store(false, Ordering::SeqCst);
    shared.fail_all(AgentError::NotAvailable("pi exited".into()));
}

async fn log_stderr<R: AsyncBufRead + Unpin>(mut reader: R, pid: Option<u32>) {
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line, MAX_STDERR_LINE_BYTES).await {
            Ok(Line::Record) => {
                tracing::debug!(?pid, line = %String::from_utf8_lossy(&line).trim_end(), "pi stderr");
            }
            Ok(Line::TooLong) => {
                // Skip the chunk that overflowed; the rest arrives as more lines.
                line.clear();
                if let Ok(buf) = reader.fill_buf().await {
                    let n = buf.len().min(MAX_STDERR_LINE_BYTES);
                    reader.consume(n);
                }
                tracing::debug!(?pid, "pi stderr line too long; dropped");
            }
            Ok(Line::Eof) | Err(_) => return,
        }
    }
}

/// Live children keyed by Pi session id (asid).
#[derive(Default)]
pub struct PiHost {
    children: Mutex<HashMap<String, Arc<PiChild>>>,
}

impl PiHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// The live child for `asid`. A child that exited on its own is dropped
    /// here, so the caller spawns a fresh one instead of writing to a corpse.
    pub fn get(&self, asid: &str) -> Option<Arc<PiChild>> {
        let mut children = self.children.lock().ok()?;
        match children.get(asid) {
            Some(child) if child.is_alive() => Some(child.clone()),
            Some(_) => {
                children.remove(asid);
                None
            }
            None => None,
        }
    }

    /// Hold `child` for `asid`, returning the one it replaces (the caller
    /// closes it).
    pub fn insert(&self, asid: String, child: Arc<PiChild>) -> Option<Arc<PiChild>> {
        self.children.lock().ok()?.insert(asid, child)
    }

    pub fn any_alive(&self) -> bool {
        self.children
            .lock()
            .is_ok_and(|children| children.values().any(|child| child.is_alive()))
    }

    pub fn remove(&self, asid: &str) -> Option<Arc<PiChild>> {
        self.children.lock().ok()?.remove(asid)
    }

    /// Close children idle for at least `timeout`, except those in `keep`,
    /// and drop dead ones. Returns the asids that were let go.
    pub async fn reap_idle(
        &self,
        timeout: Duration,
        keep: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        let reaped: Vec<(String, Arc<PiChild>)> = match self.children.lock() {
            Ok(mut children) => {
                let ids: Vec<String> = children
                    .iter()
                    .filter(|(asid, child)| {
                        !child.is_alive() || (child.idle_for() >= timeout && !keep.contains(*asid))
                    })
                    .map(|(asid, _)| asid.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|asid| children.remove(&asid).map(|child| (asid, child)))
                    .collect()
            }
            Err(_) => return Vec::new(),
        };
        close_all(reaped.iter().map(|(_, child)| child.clone()).collect()).await;
        reaped.into_iter().map(|(asid, _)| asid).collect()
    }

    pub async fn shutdown_all(&self) {
        let children: Vec<Arc<PiChild>> = match self.children.lock() {
            Ok(mut children) => children.drain().map(|(_, child)| child).collect(),
            Err(_) => return,
        };
        close_all(children).await;
    }
}

/// Close concurrently: each child may take the full grace period.
async fn close_all(children: Vec<Arc<PiChild>>) {
    let mut tasks = tokio::task::JoinSet::new();
    for child in children {
        tasks.spawn(async move { child.close().await });
    }
    while tasks.join_next().await.is_some() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Answers `get_state` (CRLF-terminated, with argv/cwd/env echoed),
    /// `prompt` (then one `agent_settled` event), exits on `die`, writes an
    /// oversized record on `flood`, and rejects anything else. Startup noise
    /// (empty line, non-object, id-less response) must be skipped.
    const FAKE_PI: &str = r#"#!/usr/bin/env python3
import json, os, sys
def out(rec, end="\n"):
    sys.stdout.write(json.dumps(rec) + end); sys.stdout.flush()
sys.stdout.write("\n[1, 2]\n"); out({"type": "response", "command": "parse", "success": False, "error": "x"})
for raw in sys.stdin.buffer:
    cmd = json.loads(raw)
    t, rid = cmd.get("type"), cmd.get("id")
    if t == "get_state":
        out({"id": rid, "type": "response", "command": t, "success": True,
             "data": {"sessionId": "s-1", "argv": sys.argv[1:], "cwd": os.getcwd(),
                      "approvals": os.environ.get("MUQUN_PI_APPROVALS")}}, "\r\n")
    elif t == "prompt":
        out({"id": rid, "type": "response", "command": t, "success": True, "data": {"disposition": "started"}})
        out({"type": "agent_settled"})
    elif t == "die":
        sys.exit(3)
    elif t == "flood":
        sys.stdout.write("x" * (17 * 1024 * 1024)); sys.stdout.flush()
    else:
        out({"id": rid, "type": "response", "command": t, "success": False, "error": "Unknown command: " + t})
"#;

    fn fake_pi() -> Option<(PiCommandLine, PiLaunch)> {
        let python = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if !python.is_ok_and(|out| out.status.success()) {
            eprintln!("python3 missing; skipping");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("muqun-pi-rpc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let binary = dir.join("pi");
        std::fs::write(&binary, FAKE_PI).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cmd = PiCommandLine {
            binary,
            extension: PathBuf::from("/workspace/demo/muqun-extension.js"),
        };
        let launch = PiLaunch {
            session_file: None,
            cwd: dir,
            env: vec![("MUQUN_PI_APPROVALS".into(), "ask".into())],
            no_extensions: false,
        };
        Some((cmd, launch))
    }

    #[tokio::test]
    async fn requests_events_and_exit() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = PiChild::spawn(&cmd, &launch).await.unwrap();
        let mut events = child.events();

        let state = child.request(json!({"type": "get_state"})).await.unwrap();
        assert_eq!(state["sessionId"], "s-1");
        assert_eq!(
            state["argv"],
            json!(["--mode", "rpc", "-e", "/workspace/demo/muqun-extension.js"])
        );
        assert_eq!(state["cwd"], launch.cwd.to_string_lossy().as_ref());
        assert_eq!(state["approvals"], "ask");

        let started = child
            .request(json!({"type": "prompt", "message": "hi"}))
            .await
            .unwrap();
        assert_eq!(started["disposition"], "started");
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event["type"], "agent_settled");

        match child.request(json!({"type": "bogus"})).await {
            Err(AgentError::RequestFailed(msg)) => assert_eq!(msg, "Unknown command: bogus"),
            other => panic!("unexpected {other:?}"),
        }

        // A request in flight when the child exits fails, and so do later ones.
        for command in ["die", "get_state"] {
            match child.request(json!({"type": command})).await {
                Err(AgentError::NotAvailable(_)) => {}
                other => panic!("{command}: unexpected {other:?}"),
            }
        }
        assert!(!child.is_alive());
    }

    #[tokio::test]
    async fn oversized_record_kills_child_and_host_reaps() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        match child.request(json!({"type": "flood"})).await {
            Err(AgentError::Protocol(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
        let host = PiHost::new();
        host.insert("s-dead".into(), child);
        let idle = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        host.insert("s-idle".into(), idle.clone());

        let mut reaped = host.reap_idle(Duration::ZERO, &Default::default()).await;
        reaped.sort();
        assert_eq!(reaped, ["s-dead", "s-idle"]);
        assert!(!idle.is_alive());
        assert!(host.get("s-idle").is_none());
    }
}
