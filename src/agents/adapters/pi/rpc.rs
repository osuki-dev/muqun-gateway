//! Process host for `pi --mode rpc`: one child per live Pi session.
//!
//! Pi's RPC mode is strict JSONL on stdin/stdout (`docs/rpc.md` in the Pi
//! install). Records are split on LF only -- a generic line reader that also
//! breaks on U+2028/U+2029 would cut JSON strings apart -- and an optional CR
//! before the LF is dropped. Commands carry a string `id` that the matching
//! `response` repeats; everything else on stdout is a session or extension-UI
//! event is fanned out to subscribers. Uploaded image bytes are removed from
//! user-message telemetry after Pi has accepted them; native session files
//! and model input retain the pixels. Stderr is never parsed.
//!
//! The host only holds children; the driver decides when to start one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};

use crate::agents::ports::agent::AgentError;

/// A record this long is not a protocol message we can use: holding it would
/// let one runaway tool output pin unbounded memory, and resynchronising
/// mid-stream is guesswork, so the child is treated as broken.
pub(super) const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// A blocked pipe must not hold shutdown or an approval response forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
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
const EVENT_CAPACITY: usize = 32;
/// One budget shared across every child's reader and the driver's queue.
/// Arc clones retain the reservation until both queues release a record.
const EVENT_BYTES: usize = 16 * 1024 * 1024;
static EVENT_BUDGET: OnceLock<Arc<Semaphore>> = OnceLock::new();

#[derive(Debug)]
pub struct PiRecord {
    value: Value,
    _budget: Option<OwnedSemaphorePermit>,
}

impl std::ops::Deref for PiRecord {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl PiRecord {
    pub(super) fn bounded(value: Value, bytes: usize) -> Option<Arc<Self>> {
        if bytes > MAX_LINE_BYTES {
            return None;
        }
        let budget = EVENT_BUDGET.get_or_init(|| Arc::new(Semaphore::new(EVENT_BYTES)));
        let permit = budget
            .clone()
            .try_acquire_many_owned(bytes.max(1).try_into().ok()?)
            .ok()?;
        Some(Arc::new(Self {
            value,
            _budget: Some(permit),
        }))
    }

    /// Small, Gateway-authored cleanup notifications must remain deliverable
    /// when upstream records have consumed the budget.
    pub(super) fn cleanup(value: Value) -> Arc<Self> {
        Arc::new(Self {
            value,
            _budget: None,
        })
    }
}

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
    /// Official exact-ID creation for a Gateway-owned, never-used session
    /// whose setup-only state Pi intentionally has not written yet.
    pub session_id: Option<String>,
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
        } else if let Some(id) = &self.session_id {
            args.push("--session-id".into());
            args.push(id.into());
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
    events: broadcast::Sender<Arc<PiRecord>>,
    alive: Arc<AtomicBool>,
    activity: Arc<Mutex<Activity>>,
    kill: Arc<Notify>,
    exited: watch::Receiver<bool>,
}

struct Activity {
    pid: Option<u32>,
    last_used: Instant,
    operations: usize,
    retiring: bool,
    running: bool,
    compacting: bool,
    queued: bool,
    has_conversation: bool,
    /// Pi exposes no active-job query. Once this owned generation executes
    /// tools or extensions, a settled model cannot prove their jobs ended.
    execution_observed: bool,
    first_execution: Option<ExecutionReason>,
    last_refusal: Option<RetirementRefusal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionReason {
    NativeTool,
    NativeBash,
    SlashCommand,
    ExtensionUi(UiMethod),
    EntryAppended,
    ExtensionError,
    CustomMessage,
    HandledInput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiMethod {
    Notify,
    SetStatus,
    SetWidget,
    SetTitle,
    SetEditorText,
    Confirm,
    Select,
    Input,
    Editor,
    Other,
}

impl UiMethod {
    fn from_record(record: &Value) -> Self {
        match record.get("method").and_then(Value::as_str) {
            Some("notify") => Self::Notify,
            Some("setStatus") => Self::SetStatus,
            Some("setWidget") => Self::SetWidget,
            Some("setTitle") => Self::SetTitle,
            Some("set_editor_text") => Self::SetEditorText,
            Some("confirm") => Self::Confirm,
            Some("select") => Self::Select,
            Some("input") => Self::Input,
            Some("editor") => Self::Editor,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RetirementRefusal {
    Retiring,
    ActivityChanged,
    InFlight,
    Running,
    Compacting,
    Queued,
    Execution,
    RecentActivity,
    PendingPermission,
    UnpersistedRows,
    SessionFileUnavailable,
}

impl Activity {
    fn observe_execution(&mut self, reason: ExecutionReason) {
        self.execution_observed = true;
        if self.first_execution.is_none() {
            self.first_execution = Some(reason);
            tracing::debug!(
                target: "muqun_gateway::idle_retirement",
                pid = ?self.pid, ?reason,
                "Pi idle retirement protected by observed execution"
            );
        }
    }

    fn refuse(&mut self, reason: RetirementRefusal) {
        if self.last_refusal != Some(reason) {
            tracing::debug!(target: "muqun_gateway::idle_retirement", pid = ?self.pid, ?reason, first_execution = ?self.first_execution, "Pi idle retirement refused");
            self.last_refusal = Some(reason);
        }
    }
}

/// An explicit session operation pins its child through all of its RPCs.
/// Acquiring and retiring use the same lock, so neither can overtake the other.
pub(super) struct PiLease {
    child: Arc<PiChild>,
}

impl std::ops::Deref for PiLease {
    type Target = PiChild;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl Drop for PiLease {
    fn drop(&mut self) {
        self.child.finish_operation();
    }
}

struct Operation<'a>(&'a PiChild);

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.0.finish_operation();
    }
}

/// Cancelling a partial JSONL write makes this transport unusable. Killing
/// it prevents a later command being appended to a truncated earlier frame.
struct WriteGuard<'a> {
    child: &'a PiChild,
    complete: bool,
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.child
                .terminate("pi write interrupted; outcome unknown; not retried");
        }
    }
}

impl Drop for PiChild {
    fn drop(&mut self) {
        self.terminate("pi transport closed");
    }
}

impl PiChild {
    pub async fn spawn(cmd: &PiCommandLine, launch: &PiLaunch) -> Result<Self, AgentError> {
        let mut command = Command::new(&cmd.binary);
        command
            .args(launch.args(cmd))
            .current_dir(&launch.cwd)
            .env_remove("MUQUN_MCP_URL")
            .env_remove("MUQUN_MCP_TOKEN")
            .envs(launch.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A gateway that dies or drops the host must not leave Pi behind.
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| AgentError::NotAvailable("cannot start pi".into()))?;
        let pid = child.id();
        let (stdin, stdout, stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (i, o, e),
                _ => return Err(AgentError::NotAvailable("pi pipes unavailable".into())),
            };

        let shared = Arc::new(Shared::default());
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let alive = Arc::new(AtomicBool::new(true));
        let activity = Arc::new(Mutex::new(Activity {
            pid,
            last_used: Instant::now(),
            operations: 0,
            retiring: false,
            running: false,
            compacting: false,
            queued: false,
            has_conversation: false,
            execution_observed: false,
            first_execution: None,
            last_refusal: None,
        }));
        let kill = Arc::new(Notify::new());
        let (exited_tx, exited) = watch::channel(false);

        let reader = tokio::spawn(read_stdout(
            BufReader::new(stdout),
            pid,
            shared.clone(),
            events.clone(),
            activity.clone(),
            alive.clone(),
            kill.clone(),
        ));
        tokio::spawn(log_stderr(BufReader::new(stderr), pid));
        {
            let shared = shared.clone();
            let alive = alive.clone();
            let kill = kill.clone();
            tokio::spawn(async move {
                let status = wait_owned_child(&mut child, &kill).await;
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
            activity,
            kill,
            exited,
        })
    }

    /// Send a command, await its `response` (matched by `id`), 60 s timeout.
    /// `Ok(data)` (`Null` when the response has none) or `RequestFailed(error)`.
    pub async fn request(&self, mut command: Value) -> Result<Value, AgentError> {
        self.begin_operation(false)?;
        let _operation = Operation(self);
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
                tracing::warn!(pid = ?self.pid, "pi request timed out");
                self.terminate("pi request timed out; outcome unknown; not retried");
                Err(AgentError::Network(
                    "pi request timed out; outcome unknown; not retried".into(),
                ))
            }
        }
    }

    /// Fire a record with no response (`extension_ui_response`).
    pub async fn send(&self, record: Value) -> Result<(), AgentError> {
        self.begin_operation(true)?;
        let _operation = Operation(self);
        self.write(&record).await
    }

    /// Every non-response stdout record. Subscribe before sending a `prompt`,
    /// or a fast run can settle before the subscriber exists.
    pub fn events(&self) -> broadcast::Receiver<Arc<PiRecord>> {
        self.events.subscribe()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(super) fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Explicit user activity; observational commands and responses do not
    /// extend the idle deadline.
    pub fn touch(&self) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.last_used = Instant::now();
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.activity
            .lock()
            .map(|activity| activity.last_used.elapsed())
            .unwrap_or_default()
    }

    pub(super) fn has_conversation(&self) -> bool {
        self.activity.lock().map_or(true, |a| a.has_conversation)
    }

    pub(super) fn note_refusal(&self, reason: RetirementRefusal) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.refuse(reason);
        }
    }

    #[cfg(test)]
    pub(super) fn diagnostics(&self) -> Value {
        self.activity.lock().map(|a| serde_json::json!({
            "idle_secs":a.last_used.elapsed().as_secs_f64(), "operations":a.operations,
            "running":a.running, "compacting":a.compacting, "queued":a.queued,
            "execution_observed":a.execution_observed, "first_execution":format!("{:?}",a.first_execution),
            "refusal":format!("{:?}",a.last_refusal)
        })).unwrap_or(Value::Null)
    }

    fn begin_operation(&self, foreground: bool) -> Result<(), AgentError> {
        let mut activity = self
            .activity
            .lock()
            .map_err(|_| AgentError::NotAvailable("pi activity poisoned".into()))?;
        if activity.retiring || !self.is_alive() {
            return Err(AgentError::NotAvailable("pi session is retiring".into()));
        }
        activity.operations += 1;
        if foreground {
            activity.last_used = Instant::now();
        }
        Ok(())
    }

    fn finish_operation(&self) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.operations = activity.operations.saturating_sub(1);
        }
    }

    pub(super) fn lease(self: &Arc<Self>) -> Option<PiLease> {
        self.begin_operation(true).ok()?;
        Some(PiLease {
            child: self.clone(),
        })
    }

    fn retire_if_idle(&self, timeout: Duration, observed: Option<Instant>) -> bool {
        self.activity.lock().is_ok_and(|mut activity| {
            let reason = if activity.retiring {
                Some(RetirementRefusal::Retiring)
            } else if observed.is_some_and(|at| at != activity.last_used) {
                Some(RetirementRefusal::ActivityChanged)
            } else if activity.operations > 0 {
                Some(RetirementRefusal::InFlight)
            } else if activity.running {
                Some(RetirementRefusal::Running)
            } else if activity.compacting {
                Some(RetirementRefusal::Compacting)
            } else if activity.queued {
                Some(RetirementRefusal::Queued)
            } else if activity.execution_observed {
                Some(RetirementRefusal::Execution)
            } else if activity.last_used.elapsed() < timeout {
                Some(RetirementRefusal::RecentActivity)
            } else {
                None
            };
            if let Some(reason) = reason {
                activity.refuse(reason);
                return false;
            }
            activity.last_refusal = None;
            activity.retiring = true;
            true
        })
    }

    /// Close stdin so Pi shuts down in order, wait up to 5 s, then kill.
    pub async fn close(&self) {
        let mut exited = self.exited.clone();
        if tokio::time::timeout(CLOSE_GRACE, async {
            self.stdin.lock().await.take();
            let _ = exited.wait_for(|done| *done).await;
        })
        .await
        .is_err()
        {
            tracing::debug!(pid = ?self.pid, "pi ignored stdin close; killing");
            self.terminate("pi transport closed");
            let _ = tokio::time::timeout(DRAIN_GRACE, exited.wait_for(|done| *done)).await;
        }
    }

    pub(super) fn terminate(&self, reason: &'static str) {
        self.alive.store(false, Ordering::SeqCst);
        self.shared
            .fail_all(AgentError::NotAvailable(reason.into()));
        self.kill.notify_one();
    }

    async fn write(&self, record: &Value) -> Result<(), AgentError> {
        let mut line = serde_json::to_vec(record)
            .map_err(|_| AgentError::InvalidRequest("invalid pi record".into()))?;
        line.push(b'\n');
        if line.len() > MAX_LINE_BYTES {
            return Err(AgentError::InvalidRequest(
                "pi command exceeds record limit".into(),
            ));
        }
        let kind = record.get("type").and_then(Value::as_str);
        let executable = kind == Some("bash")
            || (matches!(kind, Some("prompt" | "steer" | "follow_up"))
                && record
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|message| message.trim_start().starts_with('/')));
        if executable {
            if let Ok(mut activity) = self.activity.lock() {
                activity.observe_execution(if kind == Some("bash") {
                    ExecutionReason::NativeBash
                } else {
                    ExecutionReason::SlashCommand
                });
            }
        }
        if !record
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.starts_with("get_"))
        {
            self.touch();
        }
        let written = async {
            let mut stdin = self.stdin.lock().await;
            if !self.is_alive() {
                return Err(AgentError::NotAvailable("pi transport closed".into()));
            }
            let Some(pipe) = stdin.as_mut() else {
                return Err(AgentError::NotAvailable("pi stdin closed".into()));
            };
            let mut guard = WriteGuard {
                child: self,
                complete: false,
            };
            pipe.write_all(&line).await.map_err(|_| {
                AgentError::NotAvailable(
                    "pi stdin write failed; outcome unknown; not retried".into(),
                )
            })?;
            pipe.flush().await.map_err(|_| {
                AgentError::NotAvailable(
                    "pi stdin write failed; outcome unknown; not retried".into(),
                )
            })?;
            guard.complete = true;
            Ok(())
        };
        match tokio::time::timeout(WRITE_TIMEOUT, written).await {
            Ok(result) => result,
            Err(_) => {
                self.terminate("pi stdin write timed out; outcome unknown; not retried");
                Err(AgentError::NotAvailable(
                    "pi stdin write timed out; outcome unknown; not retried".into(),
                ))
            }
        }
    }
}

/// Observe exit without reaping before group cleanup, so the owned leader's
/// PID cannot be reused between inspection and killpg.
async fn wait_owned_child(
    child: &mut tokio::process::Child,
    kill: &Notify,
) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Some(group) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        let owned = tokio::select! {
            _ = kill.notified() => true,
            result = wait_unreaped(group) => result.is_ok() || result.err().is_some_and(|e| e.raw_os_error() != Some(libc::ECHILD)),
        };
        if owned {
            // SAFETY: process_group(0) created this group; its owned leader
            // has not been reaped and its PID cannot have been reused.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
        return child.wait().await;
    }
    tokio::select! {
        result = child.wait() => result,
        _ = kill.notified() => { let _ = child.start_kill(); child.wait().await }
    }
}

#[cfg(unix)]
async fn wait_unreaped(pid: i32) -> std::io::Result<()> {
    loop {
        let exited = {
            // SAFETY: waitid initializes the status; WNOWAIT preserves ownership.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            (unsafe { info.si_pid() }) != 0
        };
        if exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) enum Line {
    Record,
    TooLong,
    Eof,
}

/// Read up to the next LF into `line` (LF excluded). Stops with `TooLong`
/// as soon as the line passes `max`, without buffering the rest; an
/// unterminated tail at EOF still counts as a record.
pub(super) async fn read_line<R: AsyncBufRead + Unpin>(
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
    events: broadcast::Sender<Arc<PiRecord>>,
    activity: Arc<Mutex<Activity>>,
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
        let mut record = match serde_json::from_slice::<Value>(&line) {
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
            if let Ok(mut activity) = activity.lock() {
                activity.last_used = Instant::now();
                let kind = record
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if matches!(
                    kind,
                    "tool_execution_start"
                        | "tool_execution_update"
                        | "tool_execution_end"
                        | "entry_appended"
                        | "extension_error"
                ) || kind.starts_with("extension_ui_")
                    || (matches!(kind, "message_start" | "message_end")
                        && record.pointer("/message/role").and_then(Value::as_str)
                            == Some("custom"))
                {
                    let reason = match kind {
                        "entry_appended" => ExecutionReason::EntryAppended,
                        "extension_error" => ExecutionReason::ExtensionError,
                        "message_start" | "message_end" => ExecutionReason::CustomMessage,
                        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                            ExecutionReason::NativeTool
                        }
                        _ => ExecutionReason::ExtensionUi(UiMethod::from_record(&record)),
                    };
                    activity.observe_execution(reason);
                }
                if matches!(
                    record.get("type").and_then(Value::as_str),
                    Some("message_start" | "message_end")
                ) && matches!(
                    record.pointer("/message/role").and_then(Value::as_str),
                    Some("user" | "assistant")
                ) {
                    activity.has_conversation = true;
                }
                match record.get("type").and_then(Value::as_str) {
                    Some("agent_start" | "auto_retry_start") => activity.running = true,
                    Some("agent_settled") => activity.running = false,
                    Some("compaction_start") => activity.compacting = true,
                    Some("compaction_end") => activity.compacting = false,
                    Some("queue_update") => {
                        activity.queued = ["steering", "followUp"].iter().any(|key| {
                            record
                                .get(key)
                                .and_then(Value::as_array)
                                .is_some_and(|q| !q.is_empty())
                        });
                    }
                    _ => {}
                }
            }
            let queued_bytes = if omit_user_image_echo(&mut record) {
                serde_json::to_vec(&record).map_or(usize::MAX, |bytes| bytes.len())
            } else {
                line.len()
            };
            let Some(record) = PiRecord::bounded(record, queued_bytes) else {
                alive.store(false, Ordering::SeqCst);
                shared.fail_all(AgentError::Protocol("pi event byte budget exceeded".into()));
                kill.notify_one();
                return;
            };
            let _ = events.send(record);
            continue;
        }
        let Some(id) = record.get("id").and_then(Value::as_str) else {
            // A parse failure on our own output, or a command sent without
            // an id: nothing can be waiting on it.
            tracing::warn!(?pid, "pi response without id");
            continue;
        };
        let reply = if record.get("success").and_then(Value::as_bool) == Some(true) {
            // An input handler can consume plain text and continue work
            // asynchronously without emitting a tool or extension UI event.
            // Pin before releasing the command's operation lease.
            if matches!(
                record.get("command").and_then(Value::as_str),
                Some("prompt" | "steer" | "follow_up")
            ) && record.pointer("/data/disposition").and_then(Value::as_str) == Some("handled")
            {
                if let Ok(mut activity) = activity.lock() {
                    activity.observe_execution(ExecutionReason::HandledInput);
                }
            }
            Ok(record.get("data").cloned().unwrap_or(Value::Null))
        } else {
            // Upstream errors may contain prompts, tool input or secrets.
            Err(AgentError::RequestFailed(
                "pi command rejected before acceptance".into(),
            ))
        };
        if !shared.resolve(id, reply) {
            tracing::warn!(?pid, "pi response for unknown id");
        }
    }
    // Without stdout the child can answer nothing: callers that see the
    // failure must also see it dead, so they respawn rather than retry it.
    alive.store(false, Ordering::SeqCst);
    shared.fail_all(AgentError::NotAvailable("pi exited".into()));
}

pub(super) fn omit_user_image_echo(record: &mut Value) -> bool {
    let message_path = match record.get("type").and_then(Value::as_str) {
        Some("message_start" | "message_end") => "/message",
        Some("entry_appended") => "/entry/message",
        _ => return false,
    };
    let Some(message) = record.pointer_mut(message_path) else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    let before = blocks.len();
    blocks.retain(|block| block.get("type").and_then(Value::as_str) != Some("image"));
    blocks.len() != before
}

async fn log_stderr<R: AsyncBufRead + Unpin>(mut reader: R, pid: Option<u32>) {
    // Drain diagnostics without retaining or logging private upstream text.
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(bytes) => tracing::trace!(?pid, bytes, "pi diagnostics discarded"),
        }
    }
}

/// Live children keyed by Pi session id (asid).
#[derive(Default)]
pub struct PiHost {
    children: Mutex<HashMap<String, Arc<PiChild>>>,
    probes: Mutex<HashMap<u64, Arc<PiChild>>>,
    probe_ids: AtomicU64,
}

pub(super) struct PiProbe {
    host: Arc<PiHost>,
    child: Arc<PiChild>,
    id: u64,
    finished: bool,
}

impl PiProbe {
    pub(super) fn finish(&mut self) {
        if let Ok(mut probes) = self.host.probes.lock() {
            probes.remove(&self.id);
        }
        self.finished = true;
    }
}

impl Drop for PiProbe {
    fn drop(&mut self) {
        if !self.finished {
            self.child.terminate("pi probe cancelled; not retried");
        }
    }
}

impl PiHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn hold_probe(self: &Arc<Self>, child: Arc<PiChild>) -> PiProbe {
        let id = self.probe_ids.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut probes) = self.probes.lock() {
            probes.insert(id, child.clone());
        }
        PiProbe {
            host: self.clone(),
            child,
            id,
            finished: false,
        }
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

    pub(super) fn lease(&self, asid: &str) -> Option<PiLease> {
        // Hold the host lock until the activity pin is acquired. A reaper
        // cannot remove this child between lookup and the operation.
        self.children.lock().ok()?.get(asid)?.lease()
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

    pub(super) fn alive_count(&self) -> usize {
        let children = self
            .children
            .lock()
            .map(|children| children.values().filter(|c| c.is_alive()).count())
            .unwrap_or(usize::MAX);
        children.saturating_add(
            self.probes
                .lock()
                .map(|probes| probes.values().filter(|c| c.is_alive()).count())
                .unwrap_or(usize::MAX),
        )
    }

    pub fn remove(&self, asid: &str) -> Option<Arc<PiChild>> {
        self.children.lock().ok()?.remove(asid)
    }

    pub(super) fn activity_stamps(&self) -> HashMap<String, Instant> {
        self.children
            .lock()
            .map(|children| {
                children
                    .iter()
                    .filter_map(|(id, child)| {
                        child
                            .activity
                            .lock()
                            .ok()
                            .map(|a| (id.clone(), a.last_used))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Close children idle for at least `timeout`, except those in `keep`,
    /// and drop dead ones. Returns the asids that were let go.
    pub async fn reap_idle(
        &self,
        timeout: Duration,
        keep: &std::collections::HashSet<String>,
        observed: Option<&HashMap<String, Instant>>,
    ) -> Vec<String> {
        let reaped: Vec<(String, Arc<PiChild>)> = match self.children.lock() {
            Ok(mut children) => {
                let ids: Vec<String> = children
                    .iter()
                    .filter(|(asid, child)| {
                        !child.is_alive()
                            || (!keep.contains(*asid)
                                && observed.is_none_or(|stamps| stamps.contains_key(*asid))
                                && child.retire_if_idle(
                                    timeout,
                                    observed.and_then(|stamps| stamps.get(*asid).copied()),
                                ))
                    })
                    .map(|(asid, _)| asid.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|asid| children.remove(&asid).map(|child| (asid, child)))
                    .collect()
            }
            Err(_) => return Vec::new(),
        };
        let mut closing = reaped
            .iter()
            .map(|(_, child)| child.clone())
            .collect::<Vec<_>>();
        if let Ok(mut probes) = self.probes.lock() {
            let ids = probes
                .iter()
                .filter(|(_, child)| !child.is_alive() || child.retire_if_idle(timeout, None))
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            closing.extend(ids.into_iter().filter_map(|id| probes.remove(&id)));
        }
        close_all(closing).await;
        reaped.into_iter().map(|(asid, _)| asid).collect()
    }

    pub async fn shutdown_all(&self) {
        let mut children: Vec<Arc<PiChild>> = match self.children.lock() {
            Ok(mut children) => children.drain().map(|(_, child)| child).collect(),
            Err(_) => return,
        };
        if let Ok(mut probes) = self.probes.lock() {
            children.extend(probes.drain().map(|(_, child)| child));
        }
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
import json, os, sys, time, subprocess
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
    elif t in ("prompt", "steer", "follow_up") and cmd.get("message") == "handled input":
        out({"id": rid, "type": "response", "command": t, "success": True, "data": {"disposition": "handled"}})
    elif t == "prompt":
        out({"id": rid, "type": "response", "command": t, "success": True, "data": {"disposition": "started"}})
        out({"type": "agent_settled"})
    elif t == "die":
        sys.exit(3)
    elif t == "flood":
        sys.stdout.write("x" * (17 * 1024 * 1024)); sys.stdout.flush()
    elif t == "stall":
        out({"type": "stall_ready"})
        time.sleep(120)
    elif t == "spawn_exit":
        subprocess.Popen([sys.executable, "-c", "import time,pathlib; time.sleep(1); pathlib.Path('descendant-survived').touch()"])
        sys.exit(0)
    elif t == "event":
        out(cmd["event"])
        out({"id": rid, "type": "response", "command": t, "success": True})
    elif t == "background_tool":
        job = subprocess.Popen([sys.executable, "-c", "import time,pathlib; gate=pathlib.Path('release-background'); exec('while not gate.exists(): time.sleep(0.02)'); pathlib.Path('background-sentinel').write_text('completed')"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        out({"type":"tool_execution_start", "toolCallId":"owned-background", "toolName":"test-tool", "args":{}})
        out({"type":"tool_execution_end", "toolCallId":"owned-background", "toolName":"test-tool", "result":{}, "isError":False})
        out({"type":"agent_settled"})
        out({"id": rid, "type": "response", "command": t, "success": True, "data":{"pid":job.pid}})
    else:
        out({"id": rid, "type": "response", "command": t, "success": False, "error": "private-token-and-prompt: " + t})
"#;

    fn fake_pi() -> Option<(PiCommandLine, PiLaunch)> {
        let python = std::process::Command::new("python3")
            .arg("--version")
            .output();
        // The stand-in is a `#!` script, which Windows cannot execute.
        if cfg!(not(unix)) || !python.is_ok_and(|out| out.status.success()) {
            eprintln!("python3 missing; skipping");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("muqun-pi-rpc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let binary = dir.join("pi");
        std::fs::write(&binary, FAKE_PI).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cmd = PiCommandLine {
            binary,
            extension: PathBuf::from("/workspace/demo/muqun-extension.js"),
        };
        let launch = PiLaunch {
            session_file: None,
            session_id: None,
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
            Err(AgentError::RequestFailed(msg)) => {
                assert_eq!(msg, "pi command rejected before acceptance")
            }
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
    async fn observation_does_not_renew_idle_and_operation_pins_retirement() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        let host = PiHost::new();
        host.insert("s-1".into(), child.clone());
        let old = Instant::now() - Duration::from_secs(600);
        child.activity.lock().unwrap().last_used = old;
        child.request(json!({"type":"get_state"})).await.unwrap();
        assert_eq!(child.activity.lock().unwrap().last_used, old);
        let observed = host.activity_stamps();
        child.touch();
        assert!(
            host.reap_idle(Duration::ZERO, &Default::default(), Some(&observed))
                .await
                .is_empty(),
            "activity during durable-file validation invalidates retirement"
        );
        assert_eq!(
            child.activity.lock().unwrap().last_refusal,
            Some(RetirementRefusal::ActivityChanged)
        );
        let lease = host.lease("s-1").unwrap();
        child.activity.lock().unwrap().last_used = old;
        assert!(host
            .reap_idle(Duration::ZERO, &Default::default(), None)
            .await
            .is_empty());
        drop(lease);
        assert_eq!(
            host.reap_idle(Duration::ZERO, &Default::default(), None)
                .await,
            ["s-1"]
        );
        assert!(!child.is_alive());
        assert!(child.lease().is_none());
    }

    #[tokio::test]
    async fn running_compacting_and_queued_native_work_protect_idle_children() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        child.request(json!({"type":"get_state"})).await.unwrap();
        for (start, end) in [
            (
                json!({"type":"agent_start"}),
                json!({"type":"agent_settled"}),
            ),
            (
                json!({"type":"compaction_start"}),
                json!({"type":"compaction_end"}),
            ),
            (
                json!({"type":"queue_update","steering":[],"followUp":["keep queued"]}),
                json!({"type":"queue_update","steering":[],"followUp":[]}),
            ),
        ] {
            child
                .request(json!({"type":"event", "event":start}))
                .await
                .unwrap();
            assert!(!child.retire_if_idle(Duration::ZERO, None), "{start}");
            child
                .request(json!({"type":"event", "event":end}))
                .await
                .unwrap();
        }
        assert!(child.retire_if_idle(Duration::ZERO, None));
        child.close().await;
    }

    #[tokio::test]
    async fn settled_live_tool_generation_keeps_its_background_job_through_idle_reaping() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        let host = PiHost::new();
        host.insert("owned".into(), child.clone());
        child
            .request(json!({"type":"background_tool"}))
            .await
            .unwrap();
        // Later turns, compaction and cancellation cannot establish that
        // the completed tool's separate background job has also ended.
        for event in [
            json!({"type":"agent_start"}),
            json!({"type":"compaction_start"}),
            json!({"type":"compaction_end"}),
            json!({"type":"agent_settled", "aborted":true}),
        ] {
            child
                .request(json!({"type":"event", "event":event}))
                .await
                .unwrap();
        }
        assert!(host
            .reap_idle(Duration::ZERO, &Default::default(), None)
            .await
            .is_empty());
        assert!(child.is_alive());
        assert!(!launch.cwd.join("background-sentinel").exists());
        std::fs::write(launch.cwd.join("release-background"), "release").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !launch.cwd.join("background-sentinel").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("owned background job survives idle retirement attempt");
        assert_eq!(
            std::fs::read_to_string(launch.cwd.join("background-sentinel")).unwrap(),
            "completed"
        );
        host.shutdown_all().await;
    }

    #[tokio::test]
    async fn extension_commands_and_native_execution_records_latch_the_owned_generation() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        for command in [
            json!({"type":"prompt", "message":"/extension-background"}),
            json!({"type":"bash", "command":"unused fixture command"}),
            json!({"type":"event", "event":{"type":"extension_ui_request", "method":"notify"}}),
            json!({"type":"event", "event":{"type":"entry_appended", "entry":{"type":"custom"}}}),
            json!({"type":"event", "event":{"type":"tool_execution_end"}}),
            json!({"type":"event", "event":{"type":"extension_error", "extensionPath":"/fixture/extension.ts", "event":"input", "error":"handler failed"}}),
            json!({"type":"event", "event":{"type":"message_start", "message":{"role":"custom", "customType":"fixture", "content":"extension output"}}}),
            json!({"type":"event", "event":{"type":"message_end", "message":{"role":"custom", "customType":"fixture", "content":"extension output"}}}),
        ] {
            let child = PiChild::spawn(&cmd, &launch).await.unwrap();
            let _reply = child.request(command.clone()).await;
            assert!(!child.retire_if_idle(Duration::ZERO, None), "{command}");
            child.close().await;
        }
    }

    #[tokio::test]
    async fn handled_input_pins_before_reply_and_later_normal_turns_cannot_clear_it() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        for kind in ["prompt", "steer", "follow_up"] {
            let child = PiChild::spawn(&cmd, &launch).await.unwrap();
            let reply = child
                .request(json!({"type":kind, "message":"handled input"}))
                .await
                .unwrap();
            assert_eq!(reply["disposition"], "handled");
            assert_eq!(
                child.activity.lock().unwrap().first_execution,
                Some(ExecutionReason::HandledInput)
            );
            assert!(
                !child.retire_if_idle(Duration::ZERO, None),
                "handled {kind}"
            );
            child
                .request(json!({"type":"prompt", "message":"ordinary later turn"}))
                .await
                .unwrap();
            child
                .request(json!({"type":"event", "event":{"type":"agent_settled"}}))
                .await
                .unwrap();
            assert!(
                !child.retire_if_idle(Duration::ZERO, None),
                "later turn cleared handled {kind}"
            );
            assert_eq!(
                child.activity.lock().unwrap().first_execution,
                Some(ExecutionReason::HandledInput)
            );
            assert_eq!(
                child.activity.lock().unwrap().last_refusal,
                Some(RetirementRefusal::Execution)
            );
            child.close().await;
        }
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

        let mut reaped = host
            .reap_idle(Duration::ZERO, &Default::default(), None)
            .await;
        reaped.sort();
        assert_eq!(reaped, ["s-dead", "s-idle"]);
        assert!(!idle.is_alive());
        assert!(host.get("s-idle").is_none());
    }

    #[tokio::test]
    async fn cancelled_partial_write_closes_transport_and_shutdown_is_bounded() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = PiChild::spawn(&cmd, &launch).await.unwrap();
        let mut events = child.events();
        child.send(json!({"type":"stall"})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        let record =
            json!({"type":"extension_ui_response", "padding":"x".repeat(MAX_LINE_BYTES / 2)});
        assert!(
            tokio::time::timeout(Duration::from_millis(30), child.send(record))
                .await
                .is_err()
        );
        assert!(!child.is_alive());
        assert!(child.send(json!({"type":"get_state"})).await.is_err());
        tokio::time::timeout(Duration::from_secs(8), child.close())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_private_probes_remain_owned_until_shutdown_reaps_them() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let host = Arc::new(PiHost::new());
        let child = Arc::new(PiChild::spawn(&cmd, &launch).await.unwrap());
        let probe = host.hold_probe(child.clone());
        assert_eq!(host.alive_count(), 1);
        drop(probe);
        assert!(!child.is_alive());
        host.shutdown_all().await;
        assert!(*child.exited.borrow());
        assert!(host.probes.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_exit_removes_tool_descendants_before_they_can_write() {
        let Some((cmd, launch)) = fake_pi() else {
            return;
        };
        let child = PiChild::spawn(&cmd, &launch).await.unwrap();
        child.send(json!({"type":"spawn_exit"})).await.unwrap();
        let mut exited = child.exited.clone();
        tokio::time::timeout(Duration::from_secs(4), exited.wait_for(|v| *v))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert!(!launch.cwd.join("descendant-survived").exists());
    }

    #[test]
    fn event_reservations_are_shared_and_released_after_the_last_queue_copy() {
        // A dedicated budget keeps this invariant deterministic while other
        // protocol tests exercise the process-wide budget concurrently.
        let budget = Arc::new(Semaphore::new(8));
        let permit = budget.clone().try_acquire_many_owned(8).unwrap();
        let record = Arc::new(PiRecord {
            value: json!({"type":"example"}),
            _budget: Some(permit),
        });
        let forwarded = record.clone();
        drop(record);
        assert!(budget.clone().try_acquire_owned().is_err());
        drop(forwarded);
        assert_eq!(budget.available_permits(), 8);
        assert!(PiRecord::bounded(Value::Null, MAX_LINE_BYTES + 1).is_none());
    }

    #[test]
    fn uploaded_pixels_are_not_repeated_in_user_event_queues() {
        let mut record = json!({"type":"message_end","message":{"role":"user","timestamp":1,"content":[{"type":"text","text":"uploaded image path"},{"type":"image","mimeType":"image/webp","data":"a".repeat(2 * 1024 * 1024)}]}});
        assert!(omit_user_image_echo(&mut record));
        assert_eq!(
            record["message"]["content"],
            json!([{"type":"text","text":"uploaded image path"}])
        );
        assert!(serde_json::to_vec(&record).unwrap().len() < 1024);
        let mut entry = json!({"type":"entry_appended","entry":{"id":"native","message":{"role":"user","content":[{"type":"image","data":"pixels"}]}}});
        assert!(omit_user_image_echo(&mut entry));
        assert_eq!(entry["entry"]["id"], "native");
        let mut other = json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"image","data":"preserve provider output"}]}});
        assert!(!omit_user_image_echo(&mut other));
        assert!(other.pointer("/message/content/0/data").is_some());
    }
}
