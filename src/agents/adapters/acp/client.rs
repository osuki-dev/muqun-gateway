//! Bounded ACP stdio transport. Provider adapters own protocol and credential policy.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

// Each pipe, queue and map is independently bounded; providers may impose smaller limits.
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_PENDING: usize = 32;
const EVENT_CAPACITY: usize = 16;
const WRITE_CAPACITY: usize = 8;
const MAX_SERVER_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub enum AcpEvent {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Disconnected {
        reason: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcpRetirement {
    Retired,
    NotIdle,
}

struct Reply {
    sender: oneshot::Sender<Result<Value, String>>,
    arm_work_guard: bool,
}

struct State {
    provider: &'static str,
    #[cfg(test)]
    process_id: Option<u32>,
    safe_error_message: SafeErrorMessage,
    pending: Mutex<HashMap<u64, Reply>>,
    server_requests: Mutex<HashMap<String, usize>>,
    next_id: AtomicU64,
    cancel: CancellationToken,
    exited: CancellationToken,
    retiring: AtomicBool,
    work_guard_armed: AtomicBool,
    live_execution_seen: AtomicBool,
    exit_success: Mutex<Option<bool>>,
    stdout_drained: CancellationToken,
    stderr_drained: CancellationToken,
    events: broadcast::Sender<AcpEvent>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(crate) type SafeErrorMessage = fn(&Value) -> Option<&'static str>;

fn no_safe_error_message(_: &Value) -> Option<&'static str> {
    None
}

impl State {
    fn disconnected(&self) -> String {
        format!(
            "{} ACP disconnected; request outcome may be unknown; not retried",
            self.provider
        )
    }
}

impl State {
    fn stop(&self, reason: &'static str) {
        // Cancellation and draining are serialized with request admission.
        let mut pending = locked(&self.pending);
        if self.cancel.is_cancelled() {
            return;
        }
        self.cancel.cancel();
        for (_, reply) in pending.drain() {
            let _ = reply.sender.send(Err(self.disconnected()));
        }
        locked(&self.server_requests).clear();
        let _ = self.events.send(AcpEvent::Disconnected {
            reason: reason.into(),
        });
    }

    async fn emit(&self, event: AcpEvent) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err(self.disconnected());
        }
        // Backpressure preserves history replay and streamed deltas. A consumer
        // which stops draining cannot retain the child or its request forever.
        let deadline = tokio::time::Instant::now() + WRITE_TIMEOUT;
        while self.events.len() >= EVENT_CAPACITY {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(self.disconnected()),
                _ = tokio::time::sleep_until(deadline) => {
                    self.stop("event_consumer_stalled");
                    return Err(self.disconnected());
                }
                _ = tokio::time::sleep(Duration::from_millis(2)) => {},
            }
        }
        self.events.send(event).map_err(|_| {
            self.stop("event_consumer_closed");
            self.disconnected()
        })?;
        Ok(())
    }
}

enum WriteCommand {
    Packet(WritePacket),
    Close,
}

struct WritePacket {
    bytes: Vec<u8>,
    written: oneshot::Sender<Result<(), String>>,
}

/// External ownership is separate from the pipe tasks, so the last client drop
/// cancels and reaps even a child which never produces another stdout byte.
pub struct AcpClient {
    state: Arc<State>,
    outgoing: mpsc::Sender<WriteCommand>,
}

/// A request admitted only after its complete frame was flushed to agent stdin.
/// Dropping it removes correlation state; it never resubmits the operation.
pub struct AcpRequest {
    state: Arc<State>,
    id: u64,
    reply: Option<oneshot::Receiver<Result<Value, String>>>,
}

impl AcpRequest {
    pub async fn wait(self) -> Result<Value, String> {
        self.wait_with_timeout(REQUEST_TIMEOUT).await
    }

    pub async fn wait_with_timeout(mut self, timeout: Duration) -> Result<Value, String> {
        let reply = self.reply.take().expect("request waited once");
        match tokio::time::timeout(timeout, reply).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self.state.disconnected()),
            Err(_) => {
                self.state.stop("request_timeout");
                Err(format!(
                    "{} ACP request timed out; outcome may be unknown; not retried",
                    self.state.provider
                ))
            }
        }
    }
}

impl Drop for AcpRequest {
    fn drop(&mut self) {
        let abandoned = locked(&self.state.pending).remove(&self.id).is_some();
        if abandoned {
            // An HTTP caller can disappear during a partial write or a turn.
            // Closing prevents a later request from treating that turn as idle.
            self.state.stop("request_abandoned");
        }
    }
}

impl AcpClient {
    pub async fn spawn(
        binary: &str,
        args: &[&str],
        cwd: &str,
        provider: &'static str,
    ) -> Result<Arc<Self>, String> {
        Self::spawn_with_error_mapper(binary, args, cwd, provider, no_safe_error_message).await
    }

    pub(crate) async fn spawn_with_error_mapper(
        binary: &str,
        args: &[&str],
        cwd: &str,
        provider: &'static str,
        safe_error_message: SafeErrorMessage,
    ) -> Result<Arc<Self>, String> {
        Self::spawn_with_env(binary, args, &[], cwd, provider, safe_error_message).await
    }

    pub(crate) async fn spawn_with_env(
        binary: &str,
        args: &[&str],
        env: &[(String, String)],
        cwd: &str,
        provider: &'static str,
        safe_error_message: SafeErrorMessage,
    ) -> Result<Arc<Self>, String> {
        if !std::path::Path::new(cwd).is_absolute() {
            return Err(format!(
                "{provider} ACP requires an absolute working directory"
            ));
        }
        let mut command = Command::new(binary);
        command.args(args).current_dir(cwd);
        command.envs(env.iter().map(|(name, value)| (name, value)));
        Self::spawn_command(command, provider, safe_error_message)
    }

    fn spawn_command(
        mut command: Command,
        provider: &'static str,
        safe_error_message: SafeErrorMessage,
    ) -> Result<Arc<Self>, String> {
        use std::process::Stdio;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| format!("Could not start {provider} ACP"))?;
        #[cfg(unix)]
        let process_group = child.id().and_then(|pid| i32::try_from(pid).ok());
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{provider} ACP stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("{provider} ACP stdout unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("{provider} ACP stderr unavailable"))?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let state = Arc::new(State {
            provider,
            #[cfg(test)]
            process_id: child.id(),
            safe_error_message,
            pending: Mutex::new(HashMap::new()),
            server_requests: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            cancel: CancellationToken::new(),
            exited: CancellationToken::new(),
            retiring: AtomicBool::new(false),
            work_guard_armed: AtomicBool::new(false),
            live_execution_seen: AtomicBool::new(false),
            exit_success: Mutex::new(None),
            stdout_drained: CancellationToken::new(),
            stderr_drained: CancellationToken::new(),
            events,
        });
        let (outgoing, mut packets) = mpsc::channel::<WriteCommand>(WRITE_CAPACITY);
        let client = Arc::new(Self {
            state: state.clone(),
            outgoing: outgoing.clone(),
        });

        let writer = state.clone();
        tokio::spawn(async move {
            loop {
                let packet = tokio::select! {
                    biased;
                    _ = writer.cancel.cancelled() => break,
                    packet = packets.recv() => match packet { Some(packet) => packet, None => break },
                };
                let packet = match packet {
                    WriteCommand::Packet(packet) => packet,
                    // FIFO ordering lets previously admitted writes finish before
                    // EOF. Keep both readers alive while the agent flushes.
                    WriteCommand::Close => break,
                };
                let result = tokio::select! {
                    biased;
                    _ = writer.cancel.cancelled() => Err(writer.disconnected()),
                    result = tokio::time::timeout(WRITE_TIMEOUT, async {
                        stdin.write_all(&packet.bytes).await?;
                        stdin.flush().await
                    }) => match result {
                        Ok(Ok(())) => Ok(()),
                        _ => Err(format!("{} ACP write failed; outcome may be unknown; not retried", writer.provider)),
                    },
                };
                let failed = result.is_err();
                let _ = packet.written.send(result);
                if failed {
                    writer.stop("stdin_write_failed");
                    break;
                }
            }
        });

        let reader = state.clone();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                let frame = tokio::select! {
                    biased;
                    _ = reader.cancel.cancelled() => break,
                    frame = read_frame(&mut stdout) => frame,
                };
                match frame {
                    Ok(Some(frame)) => {
                        if dispatch(&reader, &outgoing, &frame).await.is_err() {
                            reader.stop("stdout_dispatch_failed");
                            break;
                        }
                    }
                    Ok(None) => {
                        if !reader.retiring.load(Ordering::Acquire) {
                            reader.stop("stdout_closed");
                        }
                        break;
                    }
                    Err(_) => {
                        reader.stop("invalid_or_oversized_stdout");
                        break;
                    }
                }
            }
            reader.stdout_drained.cancel();
        });

        let diagnostics = state.clone();
        tokio::spawn(async move {
            // Drain into a fixed scratch buffer. Native diagnostics can include
            // credentials and user content, so they never enter logs or errors.
            let mut buffer = [0_u8; 4096];
            loop {
                tokio::select! {
                    biased;
                    _ = diagnostics.cancel.cancelled() => break,
                    result = stderr.read(&mut buffer) => match result {
                        Ok(0) => break,
                        Err(_) => {
                            if diagnostics.retiring.load(Ordering::Acquire) {
                                diagnostics.stop("stderr_read_failed_during_retirement");
                            }
                            break;
                        }
                        Ok(_) => {},
                    }
                }
            }
            diagnostics.stderr_drained.cancel();
        });

        let lifecycle = state.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            if let Some(group) = process_group {
                let (still_owned, exit_observed) = tokio::select! {
                    biased;
                    _ = lifecycle.cancel.cancelled() => (true, false),
                    result = wait_for_unreaped_exit(group) => {
                        if !lifecycle.retiring.load(Ordering::Acquire) {
                            lifecycle.stop("process_exited");
                        }
                        // Only ECHILD means ownership was lost. Other inspection
                        // failures still close the owned child instead of waiting
                        // indefinitely for a process whose exit cannot be observed.
                        match result {
                            Ok(()) => (true, true),
                            Err(error) => (error.raw_os_error() != Some(libc::ECHILD), false),
                        }
                    },
                };
                let mut descendants_cleaned = false;
                if still_owned {
                    // SAFETY: process_group(0) created this group and the owned
                    // child has not been reaped, so its id cannot be reused.
                    let result = unsafe { libc::killpg(group, libc::SIGKILL) };
                    let error = (result != 0).then(std::io::Error::last_os_error);
                    descendants_cleaned = result == 0
                        || error.as_ref().and_then(std::io::Error::raw_os_error)
                            == Some(libc::ESRCH);
                    #[cfg(target_os = "macos")]
                    if !descendants_cleaned
                        && exit_observed
                        && error.as_ref().and_then(std::io::Error::raw_os_error)
                            == Some(libc::EPERM)
                    {
                        // Darwin returns EPERM for an empty/zombie-only group.
                        // Permission denial alone is not cleanup evidence: query
                        // the still-anchored group before accepting that case.
                        descendants_cleaned = exited_group_has_no_other_members(group);
                    }
                }
                *locked(&lifecycle.exit_success) = child
                    .wait()
                    .await
                    .ok()
                    .map(|status| status.success() && exit_observed && descendants_cleaned);
                lifecycle.exited.cancel();
                return;
            }
            tokio::select! {
                biased;
                _ = lifecycle.cancel.cancelled() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                status = child.wait() => {
                    *locked(&lifecycle.exit_success) = status.ok().map(|status| status.success());
                    if !lifecycle.retiring.load(Ordering::Acquire) {
                        lifecycle.stop("process_exited");
                    }
                },
            }
            lifecycle.exited.cancel();
        });
        Ok(client)
    }

    #[cfg(test)]
    pub(crate) fn process_id(&self) -> Option<u32> {
        self.state.process_id
    }

    pub fn is_running(&self) -> bool {
        !self.state.exited.is_cancelled()
    }

    /// Initial native load replay is history, not work owned by this child.
    /// Once armed, execution evidence is never cleared for this generation.
    #[cfg(test)]
    pub fn arm_live_work_guard(&self) {
        let _admission = locked(&self.state.pending);
        self.state.work_guard_armed.store(true, Ordering::Release);
    }

    pub fn has_unverified_live_work(&self) -> bool {
        self.state.live_execution_seen.load(Ordering::Acquire)
    }

    pub fn protect_unverified_live_work(&self) {
        let _admission = locked(&self.state.pending);
        self.state
            .live_execution_seen
            .store(true, Ordering::Release);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AcpEvent> {
        self.state.events.subscribe()
    }

    pub async fn start_request(&self, method: &str, params: Value) -> Result<AcpRequest, String> {
        let id = self.state.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, receiver) = oneshot::channel();
        {
            let mut pending = locked(&self.state.pending);
            if self.state.cancel.is_cancelled() || self.state.retiring.load(Ordering::Acquire) {
                return Err(self.state.disconnected());
            }
            if pending.len() >= MAX_PENDING {
                return Err(format!(
                    "{} ACP pending request limit reached",
                    self.state.provider
                ));
            }
            // A new session has no replay. For a load, arm at the matching
            // successful response, before the reader can dispatch live events.
            if method == "session/new" {
                self.state.work_guard_armed.store(true, Ordering::Release);
            }
            pending.insert(
                id,
                Reply {
                    sender: reply,
                    arm_work_guard: method == "session/load",
                },
            );
        }
        let request = AcpRequest {
            state: self.state.clone(),
            id,
            reply: Some(receiver),
        };
        send_packet(
            &self.state,
            &self.outgoing,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await?;
        Ok(request)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        self.start_request(method, params).await?.wait().await
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.start_request(method, params)
            .await?
            .wait_with_timeout(timeout)
            .await
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        send_packet(
            &self.state,
            &self.outgoing,
            json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
        .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<(), String> {
        let key = request_key(&id)
            .ok_or_else(|| format!("Invalid {} ACP request id", self.state.provider))?;
        if locked(&self.state.server_requests).remove(&key).is_none() {
            return Err(format!(
                "{} ACP approval is no longer pending",
                self.state.provider
            ));
        }
        let response = send_packet(
            &self.state,
            &self.outgoing,
            json!({"jsonrpc":"2.0","id":id,"result":result}),
        )
        .await;
        if response.is_err() {
            self.state.stop("permission_response_failed");
        }
        response
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) -> Result<(), String> {
        let key = request_key(&id)
            .ok_or_else(|| format!("Invalid {} ACP request id", self.state.provider))?;
        if locked(&self.state.server_requests).remove(&key).is_none() {
            return Err(format!(
                "{} ACP approval is no longer pending",
                self.state.provider
            ));
        }
        let response = send_packet(
            &self.state,
            &self.outgoing,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
        )
        .await;
        if response.is_err() {
            self.state.stop("server_error_response_failed");
        }
        response
    }

    /// Stop admission, close stdin in FIFO order, and wait for a normal exit
    /// with both output pipes drained. This proves process shutdown, never
    /// session durability; the provider must establish recovery separately.
    pub async fn retire(&self, timeout: Duration) -> Result<AcpRetirement, String> {
        {
            let pending = locked(&self.state.pending);
            if self.state.cancel.is_cancelled() {
                return Err(self.state.disconnected());
            }
            if self.state.retiring.load(Ordering::Acquire)
                || self.state.live_execution_seen.load(Ordering::Acquire)
                || !pending.is_empty()
                || !locked(&self.state.server_requests).is_empty()
            {
                return Ok(AcpRetirement::NotIdle);
            }
            self.state.retiring.store(true, Ordering::Release);
            if self.outgoing.try_send(WriteCommand::Close).is_err() {
                self.state.retiring.store(false, Ordering::Release);
                return Ok(AcpRetirement::NotIdle);
            }
        }
        let drained = tokio::time::timeout(timeout, async {
            self.state.exited.cancelled().await;
            self.state.stdout_drained.cancelled().await;
            self.state.stderr_drained.cancelled().await;
        })
        .await;
        if drained.is_ok()
            && !self.state.cancel.is_cancelled()
            && *locked(&self.state.exit_success) == Some(true)
        {
            return Ok(AcpRetirement::Retired);
        }
        self.state.stop("retirement_unconfirmed");
        let _ = tokio::time::timeout(WRITE_TIMEOUT, self.state.exited.cancelled()).await;
        Err(format!(
            "{} ACP retirement was not confirmed; session recovery is unknown",
            self.state.provider
        ))
    }

    pub async fn shutdown(&self) {
        self.state.stop("local_shutdown");
        let _ = tokio::time::timeout(WRITE_TIMEOUT, self.state.exited.cancelled()).await;
    }
}

/// Observe exit without reaping: the zombie leader reserves the owned group
/// id until its descendants have been killed. Waiting first would open a race
/// in which an unrelated new process group could reuse that id.
#[cfg(unix)]
async fn wait_for_unreaped_exit(pid: i32) -> std::io::Result<()> {
    loop {
        let exited = {
            // SAFETY: siginfo_t accepts all-zero initialization; waitid writes it
            // before inspection and WNOWAIT leaves the owned child unreaped.
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
            // SAFETY: waitid initialized the child-status fields above.
            (unsafe { info.si_pid() }) != 0
        };
        if exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// A full buffer may have been truncated. Only an empty result or the exact
// already-exited, unreaped leader can establish absence of live group members.
#[cfg(any(target_os = "macos", test))]
fn only_exited_leader_listed(group: i32, members: &[i32; 2], count: i32, error: i32) -> bool {
    error == 0
        && count >= 0
        && count < members.len() as i32
        && members[..count as usize].iter().all(|pid| *pid == group)
}

#[cfg(target_os = "macos")]
fn exited_group_has_no_other_members(group: i32) -> bool {
    let mut members = [0_i32; 2];
    // libproc maps an enumeration error to zero while preserving errno. Clear
    // the previous killpg error first; a failed query must never look empty.
    // SAFETY: __error returns this thread's errno; the buffer has its exact
    // advertised capacity and contains only PIDs from this owned group.
    let (count, error) = unsafe {
        *libc::__error() = 0;
        let count = libc::proc_listpgrppids(
            group,
            members.as_mut_ptr().cast(),
            std::mem::size_of_val(&members) as i32,
        );
        (count, *libc::__error())
    };
    only_exited_leader_listed(group, &members, count, error)
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.state.stop("client_dropped");
    }
}

async fn send_packet(
    state: &State,
    outgoing: &mpsc::Sender<WriteCommand>,
    value: Value,
) -> Result<(), String> {
    if state.cancel.is_cancelled() {
        return Err(state.disconnected());
    }
    let mut bounded = BoundedFrame(Vec::new());
    serde_json::to_writer(&mut bounded, &value).map_err(|_| {
        format!(
            "{} ACP outbound frame exceeds byte limit or cannot be encoded",
            state.provider
        )
    })?;
    let mut bytes = bounded.0;
    bytes.push(b'\n');
    let (written, receiver) = oneshot::channel();
    // Share admission with retirement; Close cannot overtake an admitted write.
    {
        let _admission = locked(&state.pending);
        if state.cancel.is_cancelled() || state.retiring.load(Ordering::Acquire) {
            return Err(state.disconnected());
        }
        outgoing
            .try_send(WriteCommand::Packet(WritePacket { bytes, written }))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    format!(
                        "{} ACP write queue is full; request was not queued",
                        state.provider
                    )
                }
                mpsc::error::TrySendError::Closed(_) => state.disconnected(),
            })?;
    }
    // The deadline includes queueing. On timeout a queued/partial write has an
    // ambiguous outcome, so close the connection and never attempt another write.
    match tokio::time::timeout(WRITE_TIMEOUT, receiver).await {
        Ok(Ok(result)) => result,
        _ => {
            state.stop("write_acknowledgement_failed");
            Err(state.disconnected())
        }
    }
}

struct BoundedFrame(Vec<u8>);

impl std::io::Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("ACP frame byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn request_key(id: &Value) -> Option<String> {
    match id {
        Value::String(id) => Some(format!("s:{id}")),
        Value::Number(id) if id.is_i64() || id.is_u64() => Some(format!("n:{id}")),
        _ => None,
    }
}

async fn dispatch(
    state: &State,
    outgoing: &mpsc::Sender<WriteCommand>,
    frame: &[u8],
) -> Result<(), String> {
    let value: Value = serde_json::from_slice(frame).map_err(|_| {
        state.stop("invalid_json");
        state.disconnected()
    })?;
    if value["jsonrpc"] != "2.0" || !value.is_object() {
        state.stop("invalid_jsonrpc_envelope");
        return Err(state.disconnected());
    }
    if value.get("method").is_some()
        && (value.get("method").and_then(Value::as_str).is_none()
            || value.get("result").is_some()
            || value.get("error").is_some())
    {
        state.stop("invalid_jsonrpc_method");
        return Err(state.disconnected());
    }
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        // Claude Code's adapter reports login changes this way; it is metadata.
        let passive_update = method == "_auth/status_update"
            || method == "session/update"
                && matches!(
                    params["update"]["sessionUpdate"].as_str(),
                    Some(
                        "user_message_chunk"
                            | "agent_message_chunk"
                            | "agent_thought_chunk"
                            | "plan"
                            | "notice"
                            | "usage_update"
                            | "available_commands_update"
                            | "current_mode_update"
                            | "config_option_update"
                            | "session_info_update"
                    )
                );
        // Unknown live extension/subagent/control messages cannot establish
        // job absence. Ordinary text, diagnostics and metadata do not pin.
        let execution = value.get("id").is_some() || !passive_update;
        if execution {
            // Record before event backpressure or prompt-result delivery can
            // make the reducer look idle. Share the final EOF admission lock.
            let _admission = locked(&state.pending);
            if state.work_guard_armed.load(Ordering::Acquire) {
                state.live_execution_seen.store(true, Ordering::Release);
            }
        }
        if let Some(id) = value.get("id") {
            if state.retiring.load(Ordering::Acquire) {
                state.stop("server_request_during_retirement");
                return Err(state.disconnected());
            }
            let Some(key) = request_key(id) else {
                state.stop("invalid_server_request_id");
                return Err(state.disconnected());
            };
            if method != "session/request_permission" {
                return send_packet(state, outgoing, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported by Muqun Gateway"}})).await;
            }
            {
                let admission = locked(&state.pending);
                if state.retiring.load(Ordering::Acquire) || state.cancel.is_cancelled() {
                    drop(admission);
                    state.stop("server_request_during_retirement");
                    return Err(state.disconnected());
                }
                let mut requests = locked(&state.server_requests);
                if requests.contains_key(&key)
                    || requests.len() >= MAX_PENDING
                    || requests.values().sum::<usize>() + frame.len() > MAX_SERVER_REQUEST_BYTES
                {
                    drop(requests);
                    drop(admission);
                    state.stop("server_request_limit_or_duplicate");
                    return Err(state.disconnected());
                }
                requests.insert(key, frame.len());
            }
            state
                .emit(AcpEvent::Request {
                    id: id.clone(),
                    method: method.into(),
                    params,
                })
                .await
        } else {
            state
                .emit(AcpEvent::Notification {
                    method: method.into(),
                    params,
                })
                .await
        }
    } else if let Some(id) = value.get("id").and_then(Value::as_u64) {
        let result = match (value.get("result"), value.get("error")) {
            (Some(result), None) => Ok(result.clone()),
            (None, Some(error)) if error.get("code").and_then(Value::as_i64).is_some() => {
                Err(rpc_error(state, error))
            }
            _ => {
                state.stop("invalid_jsonrpc_response");
                return Err(state.disconnected());
            }
        };
        if let Some(reply) = locked(&state.pending).remove(&id) {
            if reply.arm_work_guard && result.is_ok() {
                state.work_guard_armed.store(true, Ordering::Release);
            }
            let _ = reply.sender.send(result);
        }
        Ok(())
    } else {
        state.stop("invalid_jsonrpc_message");
        Err(state.disconnected())
    }
}

fn rpc_error(state: &State, error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let prefix = format!("{} ACP request rejected (code {code})", state.provider);
    match (state.safe_error_message)(error) {
        Some(message) => format!("{prefix}: {message}"),
        None => prefix,
    }
}

async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Vec<u8>>, ()> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            return if frame.is_empty() { Ok(None) } else { Err(()) };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |index| index + 1);
        if frame.len() + count > MAX_FRAME_BYTES + 1 {
            return Err(());
        }
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_uses_explicit_argv_cwd_and_provider_without_fx_policy() {
        let cwd = std::env::temp_dir().join(format!("muqun-acp-argv-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&cwd).unwrap();
        // macOS temp_dir may use /var while sh reports /private/var in PWD.
        // Keep the fixture's literal cwd/argv assertion on one physical path.
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let script = r#"
            test "$PWD" = "$1" || exit 1
            IFS= read -r first
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"cwdVerified":true}}'
            IFS= read -r second
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Session is busy"}}'
            IFS= read -r hold
        "#;
        let directory = cwd.to_str().unwrap();
        let client = AcpClient::spawn(
            "sh",
            &["-c", script, "fixture", directory],
            directory,
            "Qoder",
        )
        .await
        .unwrap();
        assert_eq!(
            client.request("probe", json!({})).await.unwrap()["cwdVerified"],
            true
        );
        assert_eq!(
            client.request("error", json!({})).await.unwrap_err(),
            "Qoder ACP request rejected (code -32603)"
        );
        client.shutdown().await;
        assert!(client.state.exited.is_cancelled());
        std::fs::remove_dir(cwd).unwrap();
    }

    #[cfg(unix)]
    fn fake(script: &str) -> Arc<AcpClient> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        AcpClient::spawn_command(command, "FX", no_safe_error_message).unwrap()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retirement_waits_for_eof_flush_and_does_not_kill_on_early_stdout_eof() {
        let directory =
            std::env::temp_dir().join(format!("muqun-acp-retire-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let marker = directory.join("flushed");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
            while IFS= read -r request; do :; done
            exec 1>&-
            sleep 0.2
            printf saved > "$1"
            printf final-diagnostic >&2
            exit 0
        "#,
            "fixture",
        ]);
        command.arg(&marker);
        let client = AcpClient::spawn_command(command, "FX", no_safe_error_message).unwrap();
        client.notify("before_eof", json!({})).await.unwrap();
        assert_eq!(
            client.retire(Duration::from_secs(3)).await.unwrap(),
            AcpRetirement::Retired
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "saved");
        assert_eq!(*locked(&client.state.exit_success), Some(true));
        assert!(!client.state.cancel.is_cancelled());
        assert!(client.request("after_retirement", json!({})).await.is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exited_group_membership_refuses_errors_truncation_and_any_other_process() {
        assert!(only_exited_leader_listed(42, &[0, 0], 0, 0));
        assert!(only_exited_leader_listed(42, &[42, 0], 1, 0));
        for (members, count, error) in [
            ([0, 0], 0, libc::EPERM),
            ([0, 0], 0, libc::ESRCH),
            ([0, 0], -1, 0),
            ([42, 42], 2, 0),
            ([42, 77], 2, 0),
            ([77, 0], 1, 0),
        ] {
            assert!(!only_exited_leader_listed(42, &members, count, error));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graceful_retirement_cleans_a_live_group_member_before_reaping_the_leader() {
        let directory =
            std::env::temp_dir().join(format!("muqun-acp-group-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let marker = directory.join("descendant-survived");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
            while IFS= read -r request; do :; done
            (sleep 0.5; printf survived > "$1"; sleep 30) &
            exit 0
        "#,
            "fixture",
        ]);
        command.arg(&marker);
        let client = AcpClient::spawn_command(command, "FX", no_safe_error_message).unwrap();
        assert_eq!(
            client.retire(Duration::from_secs(5)).await.unwrap(),
            AcpRetirement::Retired
        );
        assert_eq!(*locked(&client.state.exit_success), Some(true));
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            !marker.exists(),
            "a live descendant must still be killed before the leader is reaped"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retirement_refuses_pending_requests_and_permissions_without_closing_stdin() {
        let client = fake("while IFS= read -r request; do :; done");
        let pending = client.start_request("hold", json!({})).await.unwrap();
        assert_eq!(
            client.retire(Duration::from_millis(20)).await.unwrap(),
            AcpRetirement::NotIdle
        );
        assert!(!client.state.retiring.load(Ordering::Acquire));
        drop(pending);
        client.shutdown().await;
        let client = fake("while IFS= read -r request; do :; done");
        locked(&client.state.server_requests).insert("approval".into(), 1);
        assert_eq!(
            client.retire(Duration::from_millis(20)).await.unwrap(),
            AcpRetirement::NotIdle
        );
        assert!(!client.state.retiring.load(Ordering::Acquire));
        locked(&client.state.server_requests).clear();
        assert_eq!(
            client.retire(Duration::from_secs(3)).await.unwrap(),
            AcpRetirement::Retired
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_execution_is_latched_before_reply_and_never_cleared_by_later_text() {
        let client = fake(
            r#"
            IFS= read -r first
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"owned","update":{"sessionUpdate":"tool_call","status":"completed"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
            IFS= read -r second
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"owned","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
            while IFS= read -r hold; do :; done
        "#,
        );
        let _events = client.subscribe();
        client.arm_live_work_guard();
        client.request("first", json!({})).await.unwrap();
        assert!(client.has_unverified_live_work());
        client.request("second", json!({})).await.unwrap();
        assert_eq!(
            client.retire(Duration::from_secs(1)).await.unwrap(),
            AcpRetirement::NotIdle
        );
        assert!(client.is_running());
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn initial_replayed_tools_do_not_pin_a_new_native_generation() {
        let client = fake(
            r#"
            IFS= read -r load
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"owned","update":{"sessionUpdate":"tool_call","status":"completed"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
            while IFS= read -r hold; do :; done
        "#,
        );
        let _events = client.subscribe();
        client.request("session/load", json!({})).await.unwrap();
        client.arm_live_work_guard();
        assert!(!client.has_unverified_live_work());
        assert_eq!(
            client.retire(Duration::from_secs(1)).await.unwrap(),
            AcpRetirement::Retired
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn load_reply_arms_before_the_same_read_batch_can_deliver_live_tools() {
        let client = fake(
            r#"
            IFS= read -r load
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","status":"completed"}}}' '{"jsonrpc":"2.0","id":1,"result":{}}' '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","status":"completed"}}}'
            IFS= read -r barrier
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
            while IFS= read -r hold; do :; done
        "#,
        );
        let _events = client.subscribe();
        client.request("session/load", json!({})).await.unwrap();
        client.request("barrier", json!({})).await.unwrap();
        assert!(client.has_unverified_live_work());
        assert_eq!(
            client.retire(Duration::from_secs(1)).await.unwrap(),
            AcpRetirement::NotIdle
        );
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retirement_timeout_and_nonzero_exit_do_not_confirm_recovery() {
        let client = fake("while IFS= read -r request; do :; done; sleep 30");
        assert!(client.retire(Duration::from_millis(30)).await.is_err());
        assert!(client.state.cancel.is_cancelled());
        assert!(client.state.exited.is_cancelled());
        let client = fake("while IFS= read -r request; do :; done; exit 7");
        assert!(client.retire(Duration::from_secs(3)).await.is_err());
        assert_eq!(*locked(&client.state.exit_success), Some(false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn last_client_drop_reaps_a_silent_child() {
        let client = fake("IFS= read -r hold");
        let state = client.state.clone();
        drop(client);
        tokio::time::timeout(WRITE_TIMEOUT, state.exited.cancelled())
            .await
            .unwrap();
        assert!(state.cancel.is_cancelled());
        assert!(locked(&state.pending).is_empty());
        assert!(locked(&state.server_requests).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn natural_exit_kills_descendants_before_reaping_the_group_leader() {
        let directory =
            std::env::temp_dir().join(format!("muqun-acp-exit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let marker = directory.join("descendant-survived");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            r#"
                IFS= read -r first
                (sleep 0.5; printf survived > "$1"; sleep 30) &
                printf '{"jsonrpc":"2.0","id":1,"result":{"ready":true,"descendantPid":%s}}\n' "$!"
                IFS= read -r release
                exit 0
            "#,
            "fixture",
        ]);
        command.arg(&marker);
        let client = AcpClient::spawn_command(command, "FX", no_safe_error_message).unwrap();
        let ready = client.request("ready", json!({})).await.unwrap();
        assert_eq!(ready["ready"], true);
        let descendant = ready["descendantPid"].as_i64().unwrap() as i32;
        client.notify("exit", json!({})).await.unwrap();
        tokio::time::timeout(WRITE_TIMEOUT, client.state.exited.cancelled())
            .await
            .unwrap();
        // The descendant inherits stdout, so EOF cannot cause the cleanup.
        // It would write this marker after its leader exits if left alive.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let survived = marker.exists();
        if survived {
            // SAFETY: a surviving fixture is still in its explicit 30s sleep.
            // Keep a failed regression from leaving its test process behind.
            unsafe {
                libc::kill(descendant, libc::SIGKILL);
            }
        }
        std::fs::remove_dir_all(directory).unwrap();
        assert!(!survived, "ACP descendant survived its process leader");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn correlates_out_of_order_replies_and_cleans_dropped_requests() {
        let client = fake(
            r#"
            IFS= read -r first
            IFS= read -r second
            printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"second":true}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"first":true}}'
            IFS= read -r hold
        "#,
        );
        let first = client.start_request("first", json!({})).await.unwrap();
        let second = client.start_request("second", json!({})).await.unwrap();
        assert_eq!(second.wait().await.unwrap(), json!({"second":true}));
        assert_eq!(first.wait().await.unwrap(), json!({"first":true}));
        let abandoned = client.start_request("abandoned", json!({})).await.unwrap();
        drop(abandoned);
        assert!(locked(&client.state.pending).is_empty());
        client.shutdown().await;
        assert!(client.state.exited.is_cancelled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delivers_permission_and_notification_before_prompt_result() {
        let client = fake(
            r#"
            IFS= read -r prompt
            printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"isolated","update":{"sessionUpdate":"agent_message_chunk"}}}'
            printf '%s\n' '{"jsonrpc":"2.0","id":"approval","method":"session/request_permission","params":{"sessionId":"isolated"}}'
            IFS= read -r reply
            case "$reply" in
                *'"id":"approval"'*'"result"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}';;
            esac
            IFS= read -r hold
        "#,
        );
        let mut events = client.subscribe();
        let prompt = client
            .start_request("session/prompt", json!({}))
            .await
            .unwrap();
        assert!(
            matches!(events.recv().await.unwrap(), AcpEvent::Notification { method, .. } if method == "session/update")
        );
        let id = match events.recv().await.unwrap() {
            AcpEvent::Request { id, method, .. } => {
                assert_eq!(method, "session/request_permission");
                id
            }
            event => panic!("unexpected event: {event:?}"),
        };
        client
            .respond(id.clone(), json!({"outcome":{"outcome":"cancelled"}}))
            .await
            .unwrap();
        assert!(client.respond(id, json!({})).await.is_err());
        assert_eq!(prompt.wait().await.unwrap()["stopReason"], "end_turn");
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_response_closes_connection_and_clears_pending() {
        let client = fake(
            r#"
            IFS= read -r request
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32000,"message":"private"}}'
            IFS= read -r hold
        "#,
        );
        assert!(client
            .request("initialize", json!({}))
            .await
            .unwrap_err()
            .contains("disconnected"));
        assert!(locked(&client.state.pending).is_empty());
        assert!(client.state.cancel.is_cancelled());
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_unknown_server_requests_and_redacts_native_errors() {
        let client = fake(
            r#"
            IFS= read -r request
            printf '%s\n' '{"jsonrpc":"2.0","id":"native","method":"fs/read_text_file","params":{"path":"private"}}'
            IFS= read -r rejection
            case "$rejection" in
                *'"code":-32601'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"secret-native-token"}}';;
            esac
            printf '%s\n' 'secret-stderr-token' >&2
            IFS= read -r hold
        "#,
        );
        let error = client.request("initialize", json!({})).await.unwrap_err();
        assert_eq!(error, "FX ACP request rejected (code -32000)");
        assert!(!error.contains("secret"));
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_does_not_resend_and_exit_fails_pending_requests() {
        let client = fake("while IFS= read -r request; do :; done");
        let error = client
            .request_with_timeout("mutation", json!({}), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(error.contains("not retried"));
        assert!(locked(&client.state.pending).is_empty());
        assert!(client.request("read", json!({})).await.is_err());
        assert!(client.state.cancel.is_cancelled());
        client.shutdown().await;
        let client = fake("IFS= read -r request");
        match client.start_request("exits", json!({})).await {
            Ok(pending) => assert!(pending.wait().await.unwrap_err().contains("disconnected")),
            Err(error) => assert!(error.contains("disconnected")),
        }
        assert!(locked(&client.state.pending).is_empty());
        client.shutdown().await;
    }

    #[tokio::test]
    async fn stdout_reader_bounds_unterminated_frames_and_rejects_truncation() {
        let bytes = vec![b'x'; MAX_FRAME_BYTES + 2];
        let mut reader = BufReader::new(bytes.as_slice());
        assert!(read_frame(&mut reader).await.is_err());
        let mut reader = BufReader::new(&b"{\"jsonrpc\":\"2.0\"}"[..]);
        assert!(read_frame(&mut reader).await.is_err());
        let mut reader = BufReader::new(&b"{}\r\n{}\n"[..]);
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_frame(&mut reader).await.unwrap(), Some(b"{}".to_vec()));
        assert_eq!(read_frame(&mut reader).await.unwrap(), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn history_replay_applies_backpressure_without_losing_events() {
        let client = fake(
            r#"
            IFS= read -r request
            index=0
            while [ "$index" -lt 100 ]; do
                printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"isolated"}}'
                index=$((index + 1))
            done
            printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"loaded":true}}'
            IFS= read -r hold
        "#,
        );
        let mut events = client.subscribe();
        let request = client
            .start_request("session/load", json!({}))
            .await
            .unwrap();
        for _ in 0..100 {
            assert!(matches!(
                events.recv().await.unwrap(),
                AcpEvent::Notification { .. }
            ));
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(request.wait().await.unwrap()["loaded"], true);
        assert!(!client.state.cancel.is_cancelled());
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_notification_is_written_during_pending_prompt() {
        let client = fake(
            r#"
            IFS= read -r prompt
            IFS= read -r cancel
            case "$cancel" in
                *'"method":"session/cancel"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"stopReason":"cancelled"}}';;
            esac
            IFS= read -r hold
        "#,
        );
        let request = client
            .start_request("session/prompt", json!({}))
            .await
            .unwrap();
        client
            .notify("session/cancel", json!({"sessionId":"isolated"}))
            .await
            .unwrap();
        assert_eq!(request.wait().await.unwrap()["stopReason"], "cancelled");
        client.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_and_event_queues_are_bounded() {
        let client = fake("while IFS= read -r request; do :; done");
        let mut pending = Vec::new();
        for _ in 0..MAX_PENDING {
            pending.push(client.start_request("wait", json!({})).await.unwrap());
        }
        assert!(client.start_request("overflow", json!({})).await.is_err());
        drop(pending);
        assert!(locked(&client.state.pending).is_empty());
        assert!(client.state.cancel.is_cancelled());
        client.shutdown().await;
        let client = fake("while IFS= read -r request; do :; done");
        let _events = client.subscribe();
        for _ in 0..EVENT_CAPACITY {
            client
                .state
                .emit(AcpEvent::Notification {
                    method: "test".into(),
                    params: Value::Null,
                })
                .await
                .unwrap();
        }
        assert!(client
            .state
            .emit(AcpEvent::Notification {
                method: "overflow".into(),
                params: Value::Null
            })
            .await
            .is_err());
        assert!(client.state.cancel.is_cancelled());
        client.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "requires explicit ACP_TEST_BINARY and ACP_TEST_ARGS_JSON; read-only native ACP handshake"]
    async fn real_agent_initialize_and_list_without_prompt() {
        let binary = std::env::var("ACP_TEST_BINARY").expect("explicit ACP_TEST_BINARY required");
        let args: Vec<String> = serde_json::from_str(
            &std::env::var("ACP_TEST_ARGS_JSON").expect("explicit ACP_TEST_ARGS_JSON required"),
        )
        .unwrap();
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        let cwd =
            std::env::temp_dir().join(format!("muqun-acp-read-only-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&cwd).unwrap();
        let client = AcpClient::spawn(&binary, &args, cwd.to_str().unwrap(), "ACP test")
            .await
            .unwrap();
        let mut events = client.subscribe();
        async fn read_only_request(
            client: &AcpClient,
            events: &mut broadcast::Receiver<AcpEvent>,
            method: &str,
            params: Value,
        ) -> Value {
            let reply = client.request_with_timeout(method, params, Duration::from_secs(30));
            tokio::pin!(reply);
            loop {
                tokio::select! {
                    result = &mut reply => return result.unwrap(),
                    event = events.recv() => match event.unwrap() {
                        AcpEvent::Notification { .. } => {},
                        AcpEvent::Request { .. } => panic!("Unexpected interaction; no approval sent"),
                        AcpEvent::Disconnected { reason } => panic!("ACP disconnected during read-only probe: {reason}"),
                    }
                }
            }
        }
        let init = read_only_request(&client, &mut events, "initialize", json!({
            "protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"muqun-gateway-read-only-test","version":"test"}
        })).await;
        assert_eq!(init["protocolVersion"], 1);
        let list =
            read_only_request(&client, &mut events, "session/list", json!({"cwd":cwd})).await;
        assert!(list["sessions"].is_array());
        client.shutdown().await;
        assert!(client.state.exited.is_cancelled());
        std::fs::remove_dir(cwd).unwrap();
    }
}
