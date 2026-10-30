//! `AgentPort` over local `pi --mode rpc` children.
//!
//! A Pi session is a session file and its header `id` is the session id.
//! Reads (projects, sessions, history) come from the files without a child;
//! a child is started lazily for the first write to a session and closed
//! again after `idle_timeout` without activity. Every record a child prints
//! is forwarded, tagged with its session, on one broadcast channel that the
//! manager folds (`agents/manager/pi.rs`) through [`PiDriver::fold`], so the
//! live state the driver answers from (status, pending approvals, rows not
//! yet in the file) and the mirror are fed by the same fold.
//!
//! Pi 1.4.0 sends no entry id for a live message, so after each `turn_end`
//! and `agent_settled` the forwarder reads the new tail of the session file
//! and feeds every new message entry back as a synthetic `entry_appended`;
//! the live folder re-keys its provisional `live-<n>` rows to the real ids.
//!
//! Everything Pi does not offer keeps the trait default and answers
//! `Unsupported`; nothing here returns `Ok(())` for work it did not do.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::mapper::{self, LiveChange, LiveFolder};
use super::rpc::{PiChild, PiCommandLine, PiHost, PiLaunch};
use super::sessions::{self, SessionFile, SessionStore};
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, FormRequest,
    ModelRef, PermissionDecision, PermissionRequest, SessionQuery, TimelineItem,
};
use crate::agents::ports::agent::{
    append_attachment_paths, AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem,
};

/// Discovery asks for the catalog on every refresh; a probe child costs a
/// Node start-up, so the answer is reused this long.
const CATALOG_TTL: Duration = Duration::from_secs(60);
/// `pi --version` is a process too; health checks run every 15 s.
const PROBE_TTL: Duration = Duration::from_secs(60);
/// How often the reaper looks for idle children.
const REAP_TICK: Duration = Duration::from_secs(30);
/// How often a forwarder with nothing to forward checks its child is alive,
/// so a crash mid-run does not leave the session Busy forever.
const ALIVE_TICK: Duration = Duration::from_secs(2);
/// Raw child records waiting for the manager. Deltas come in bursts; a lag
/// is logged and the stream goes on (the next snapshot read repairs it).
const EVENT_CAPACITY: usize = 4096;
/// Live rows kept per session for the timeline merge. Rows that reached the
/// file are dropped at the next read; this only bounds a session whose file
/// cannot be read.
const MAX_LIVE_ROWS: usize = 512;

/// What the driver knows about a session while (or since) a child ran it.
struct LiveSession {
    folder: LiveFolder,
    status: AgentSessionStatus,
    pending: Vec<PermissionRequest>,
    /// Rows the fold produced, keyed by row id, for the timeline merge.
    rows: Vec<TimelineItem>,
    /// The session file; `None` until Pi has said where (a new session).
    file: Option<PathBuf>,
    cwd: String,
    /// A name set while the session has no file to hold it yet.
    title: Option<String>,
    /// Bytes of the file already seen, so the tail read is cheap.
    offset: u64,
    created_ms: u64,
    /// Which child this state belongs to, so the forwarder of a child that
    /// died cannot settle the run of the one that replaced it.
    generation: u64,
}

impl LiveSession {
    fn new(asid: &AgentSessionId, file: Option<PathBuf>, cwd: String, history_len: usize) -> Self {
        Self {
            folder: LiveFolder::new(asid.clone()).with_history_len(history_len),
            status: AgentSessionStatus::Idle,
            pending: Vec::new(),
            rows: Vec::new(),
            file,
            cwd,
            title: None,
            offset: 0,
            created_ms: now_ms(),
            generation: 0,
        }
    }

    fn running(&self) -> bool {
        matches!(
            self.status,
            AgentSessionStatus::Busy | AgentSessionStatus::Retry
        )
    }
}

/// State the forwarder and reaper tasks share with the driver.
struct Live {
    events: broadcast::Sender<(AgentSessionId, Value)>,
    sessions: Mutex<HashMap<String, LiveSession>>,
    generations: std::sync::atomic::AtomicU64,
}

impl Live {
    fn publish(&self, asid: &AgentSessionId, record: Value) {
        // No subscriber yet (before the manager attaches) is not a failure.
        let _ = self.events.send((asid.clone(), record));
    }

    fn with<R>(&self, asid: &str, f: impl FnOnce(&mut LiveSession) -> R) -> Option<R> {
        let mut sessions = self.sessions.lock().ok()?;
        sessions.get_mut(asid).map(f)
    }

    fn status(&self, asid: &str) -> Option<AgentSessionStatus> {
        self.with(asid, |s| s.status)
    }

    /// Message entries appended to the session file since the last read.
    /// Only whole lines are consumed: Pi may be mid-append on the last one.
    async fn read_tail(&self, asid: &str) -> Vec<Value> {
        let Some((Some(path), offset)) = self.with(asid, |s| (s.file.clone(), s.offset)) else {
            return Vec::new();
        };
        let Ok(mut file) = tokio::fs::File::open(&path).await else {
            return Vec::new();
        };
        if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if let Err(error) = file.read_to_end(&mut bytes).await {
            tracing::debug!(%error, "pi session tail unreadable");
            return Vec::new();
        }
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        self.with(asid, |s| s.offset = offset + end as u64 + 1);
        bytes[..end]
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
            .collect()
    }
}

pub struct PiDriver {
    cmd: PiCommandLine,
    store: SessionStore,
    host: Arc<PiHost>,
    /// The child environment: the Muqun MCP variables when MCP is on, and
    /// `MUQUN_PI_APPROVALS` for the bundled extension.
    env: Vec<(String, String)>,
    idle_timeout: Duration,
    live: Arc<Live>,
    /// One child per session: two writes racing to start one must not both.
    spawn_lock: tokio::sync::Mutex<()>,
    catalog: tokio::sync::Mutex<Option<(Instant, AgentCatalog)>>,
    probed: Mutex<Option<(Instant, bool)>>,
    stop: CancellationToken,
}

impl PiDriver {
    /// `approvals`: whether the extension asks before `bash`/`edit`/`write`.
    /// `env`: extra child environment (the Muqun MCP variables).
    pub fn new(
        cmd: PiCommandLine,
        store: SessionStore,
        approvals: bool,
        idle_timeout: Duration,
        mut env: Vec<(String, String)>,
    ) -> Self {
        env.push((
            "MUQUN_PI_APPROVALS".into(),
            if approvals { "ask" } else { "never" }.into(),
        ));
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            cmd,
            store,
            host: Arc::new(PiHost::new()),
            env,
            idle_timeout,
            live: Arc::new(Live {
                events,
                sessions: Mutex::new(HashMap::new()),
                generations: std::sync::atomic::AtomicU64::new(0),
            }),
            spawn_lock: tokio::sync::Mutex::new(()),
            catalog: tokio::sync::Mutex::new(None),
            probed: Mutex::new(None),
            stop: CancellationToken::new(),
        }
    }

    /// Every record any child prints, tagged with its session, plus the
    /// synthetic ones the driver adds (re-key entries, the replies it sent).
    pub fn subscribe(&self) -> broadcast::Receiver<(AgentSessionId, Value)> {
        self.live.events.subscribe()
    }

    /// Cancelled by [`PiDriver::shutdown`], for tasks that must end with it.
    pub fn stopped(&self) -> CancellationToken {
        self.stop.clone()
    }

    /// Start closing idle children. A child whose session is running is
    /// touched first: a long silent tool call prints nothing, and the host
    /// cannot tell that from idleness.
    pub fn start_reaper(&self) {
        let (host, live, stop, timeout) = (
            self.host.clone(),
            self.live.clone(),
            self.stop.clone(),
            self.idle_timeout,
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_TICK);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let running: Vec<String> = live
                    .sessions
                    .lock()
                    .map(|s| {
                        s.iter()
                            .filter(|(_, s)| s.running())
                            .map(|(k, _)| k.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                for asid in running {
                    if let Some(child) = host.get(&asid) {
                        child.touch();
                    }
                }
                for asid in host.reap_idle(timeout).await {
                    tracing::debug!(%asid, "closed an idle pi child");
                }
            }
        });
    }

    /// Kill every child. Called from the manager's synchronous shutdown hook,
    /// so the closing runs on a task.
    pub fn shutdown(&self) {
        self.stop.cancel();
        let host = self.host.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { host.shutdown_all().await });
            }
            // No runtime left: dropping the host kills the children
            // (`kill_on_drop`).
            Err(_) => drop(host),
        }
    }

    /// Fold one record of `asid` into live state and return what changed.
    /// Dialogs that are not ours are answered `cancelled` here, so Pi never
    /// waits on a prompt nobody sees.
    pub async fn fold(&self, asid: &AgentSessionId, record: &Value) -> Vec<LiveChange> {
        let changes = {
            let Ok(mut sessions) = self.live.sessions.lock() else {
                return Vec::new();
            };
            let session = sessions
                .entry(asid.0.clone())
                .or_insert_with(|| LiveSession::new(asid, None, String::new(), 0));
            let changes = session.folder.fold(record);
            for change in &changes {
                apply(session, change);
            }
            changes
        };
        for change in &changes {
            if let LiveChange::UiCancel(id) = change {
                if let Some(child) = self.host.get(&asid.0) {
                    let reply =
                        json!({"type": "extension_ui_response", "id": id, "cancelled": true});
                    if let Err(error) = child.send(reply).await {
                        tracing::debug!(%error, "pi dialog cancel not delivered");
                    }
                }
            }
        }
        changes
    }

    fn launch(&self, session_file: Option<PathBuf>, cwd: &Path) -> PiLaunch {
        PiLaunch {
            session_file,
            cwd: cwd.to_path_buf(),
            env: self.env.clone(),
            no_extensions: false,
        }
    }

    /// Track `child` as the one running `asid` and forward its records.
    fn adopt(&self, asid: &AgentSessionId, child: Arc<PiChild>, mut session: LiveSession) {
        let events = child.events();
        let generation = self
            .live
            .generations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        session.generation = generation;
        if let Ok(mut sessions) = self.live.sessions.lock() {
            // A previous child's pending dialogs died with it.
            sessions.insert(asid.0.clone(), session);
        }
        if let Some(old) = self.host.insert(asid.0.clone(), child.clone()) {
            tokio::spawn(async move { old.close().await });
        }
        tokio::spawn(forward(
            self.live.clone(),
            asid.clone(),
            generation,
            child,
            events,
        ));
    }

    /// The child running `session_id`, started from its file if none is.
    async fn child_for(&self, session_id: &str) -> Result<Arc<PiChild>, AgentError> {
        if let Some(child) = self.host.get(session_id) {
            child.touch();
            return Ok(child);
        }
        let _one = self.spawn_lock.lock().await;
        if let Some(child) = self.host.get(session_id) {
            return Ok(child);
        }
        let file = self.store.find(session_id).await?;
        let cwd = PathBuf::from(&file.cwd);
        if !cwd.is_dir() {
            return Err(AgentError::WorkspaceMissing(file.cwd.clone()));
        }
        let entries = self.store.read_entries(&file).await?;
        let history_len = mapper::timeline_from_entries(&sessions::active_branch(&entries)).len();
        let offset = tokio::fs::metadata(&file.path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let child =
            Arc::new(PiChild::spawn(&self.cmd, &self.launch(Some(file.path.clone()), &cwd)).await?);
        let asid = AgentSessionId(file.id.clone());
        let mut session = LiveSession::new(&asid, Some(file.path.clone()), file.cwd, history_len);
        session.offset = offset;
        session.created_ms = file.created_ms;
        self.adopt(&asid, child.clone(), session);
        tracing::debug!(session = %asid.0, "started a pi child");
        Ok(child)
    }

    /// A session that exists only as a live child: Pi writes the file with
    /// the first message, so a fresh session has none yet.
    fn live_only_info(&self, session_id: &str) -> Option<AgentSessionInfo> {
        self.live.with(session_id, |s| {
            let file = SessionFile {
                id: session_id.to_string(),
                path: s.file.clone().unwrap_or_default(),
                cwd: s.cwd.clone(),
                created_ms: s.created_ms,
                modified_ms: s.created_ms,
            };
            let mut info = sessions::session_info(&file, &[], s.status);
            if let Some(title) = &s.title {
                info.title = title.clone();
            }
            info
        })
    }

    fn status_of(&self, session_id: &str) -> AgentSessionStatus {
        self.live
            .status(session_id)
            .unwrap_or(AgentSessionStatus::Idle)
    }

    async fn info_for(&self, file: &SessionFile) -> Result<AgentSessionInfo, AgentError> {
        let entries = self.store.read_entries(file).await?;
        Ok(sessions::session_info(
            file,
            &entries,
            self.status_of(&file.id),
        ))
    }

    async fn apply_model(&self, child: &PiChild, model: &ModelRef) -> Result<(), AgentError> {
        child
            .request(json!({"type": "set_model", "provider": model.provider_id, "modelId": model.model_id}))
            .await?;
        if let Some(level) = model.variant.as_deref().filter(|v| !v.is_empty()) {
            child
                .request(json!({"type": "set_thinking_level", "level": level}))
                .await?;
        }
        Ok(())
    }

    /// Models and thinking levels from a throwaway child that loads no
    /// extensions and writes no session.
    async fn read_catalog(&self) -> Result<AgentCatalog, AgentError> {
        let cwd = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        let launch = PiLaunch {
            session_file: None,
            cwd,
            env: Vec::new(),
            no_extensions: true,
        };
        let child = PiChild::spawn(&self.cmd, &launch).await?;
        let read = async {
            let models = child
                .request(json!({"type": "get_available_models"}))
                .await?;
            let levels = child
                .request(json!({"type": "get_available_thinking_levels"}))
                .await?;
            let state = child.request(json!({"type": "get_state"})).await?;
            Ok::<_, AgentError>(mapper::catalog(&models, &levels, state.get("model")))
        }
        .await;
        child.close().await;
        read
    }
}

/// Keep the driver's view of a session in step with one fold result.
fn apply(session: &mut LiveSession, change: &LiveChange) {
    match change {
        LiveChange::Status(status) => session.status = *status,
        LiveChange::Upsert(items) => {
            for item in items {
                match session.rows.iter_mut().find(|r| r.id == item.id) {
                    Some(row) => *row = item.clone(),
                    None => session.rows.push(item.clone()),
                }
            }
            if session.rows.len() > MAX_LIVE_ROWS {
                let excess = session.rows.len() - MAX_LIVE_ROWS;
                session.rows.drain(..excess);
            }
        }
        LiveChange::Remove(ids) => session.rows.retain(|r| !ids.contains(&r.id)),
        LiveChange::Permission(request) => {
            session.pending.retain(|p| p.id != request.id);
            session.pending.push((**request).clone());
        }
        LiveChange::PermissionResolved(id) => session.pending.retain(|p| &p.id != id),
        LiveChange::Title(title) => session.title = Some(title.clone()),
        LiveChange::Compaction { .. } | LiveChange::Usage { .. } | LiveChange::UiCancel(_) => {}
    }
}

/// Forward one child's records until it exits. After a turn or a run ends,
/// the file's new message entries follow as `entry_appended` so live rows
/// are re-keyed. A child that dies leaves no run and no dialog behind.
async fn forward(
    live: Arc<Live>,
    asid: AgentSessionId,
    generation: u64,
    child: Arc<PiChild>,
    mut events: broadcast::Receiver<Value>,
) {
    loop {
        let received = tokio::select! {
            received = events.recv() => received,
            _ = tokio::time::sleep(ALIVE_TICK) => {
                if child.is_alive() { continue } else { break }
            }
        };
        match received {
            Ok(record) => {
                let ends = matches!(
                    record.get("type").and_then(Value::as_str),
                    Some("turn_end" | "agent_settled")
                );
                live.publish(&asid, record);
                if ends {
                    for entry in live.read_tail(&asid.0).await {
                        live.publish(&asid, json!({"type": "entry_appended", "entry": entry}));
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(session = %asid.0, skipped, "pi events lagged");
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let (running, pending) = live
        .with(&asid.0, |s| {
            if s.generation != generation {
                return (false, Vec::new());
            }
            (
                s.running(),
                s.pending.iter().map(|p| p.id.clone()).collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    for id in pending {
        live.publish(
            &asid,
            json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        );
    }
    if running {
        tracing::warn!(session = %asid.0, "pi exited during a run");
        live.publish(&asid, json!({"type": "agent_settled", "aborted": true}));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl AgentPort for PiDriver {
    fn kind(&self) -> &'static str {
        super::KIND
    }

    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move {
            if let Some((at, ok)) = self.probed.lock().ok().and_then(|p| *p) {
                if at.elapsed() < PROBE_TTL {
                    return Ok(ok);
                }
            }
            let ok = crate::agents::runtime::binary_version(&self.cmd.binary)
                .await
                .is_some();
            if let Ok(mut probed) = self.probed.lock() {
                *probed = Some((Instant::now(), ok));
            }
            Ok(ok)
        })
    }

    fn list_projects(&self) -> AgentFuture<'_, Vec<AgentProject>> {
        Box::pin(async move { Ok(sessions::projects(&self.store.list(None).await?)) })
    }

    fn list_sessions<'a>(
        &'a self,
        query: &'a SessionQuery,
    ) -> AgentFuture<'a, Vec<AgentSessionInfo>> {
        Box::pin(async move {
            // Pi sessions have no parent; only an "any" or "top-level" filter
            // can match.
            if !query.keeps_parent(None) {
                return Ok(Vec::new());
            }
            let directory = query.directory.as_deref().map(|d| d.trim_end_matches('/'));
            let mut files = self.store.list(directory).await?;
            if query.order.as_deref() == Some("asc") {
                files.reverse();
            }
            let search = query
                .search
                .as_deref()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty());
            // Without a search, the limit can be applied before reading.
            if let (None, Some(limit)) = (&search, query.limit) {
                files.truncate(limit);
            }
            let mut out = Vec::new();
            // A new session has no file until its first message.
            let live_only: Vec<String> = self
                .live
                .sessions
                .lock()
                .map(|s| {
                    s.iter()
                        .filter(|(id, s)| {
                            s.file.as_deref().is_none_or(|f| !f.exists())
                                && directory.is_none_or(|d| d == s.cwd)
                                && self.host.get(id).is_some()
                        })
                        .map(|(id, _)| id.clone())
                        .collect()
                })
                .unwrap_or_default();
            out.extend(live_only.iter().filter_map(|id| self.live_only_info(id)));
            for file in &files {
                match self.info_for(file).await {
                    Ok(info) => out.push(info),
                    // Removed between the scan and the read.
                    Err(AgentError::SessionNotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            if let Some(search) = search {
                out.retain(|info| info.title.to_lowercase().contains(&search));
            }
            if let Some(limit) = query.limit {
                out.truncate(limit);
            }
            Ok(out)
        })
    }

    fn create_session<'a>(
        &'a self,
        directory: Option<&'a str>,
        model: Option<&'a ModelRef>,
        _mode: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            let dir = directory
                .map(|d| d.trim().trim_end_matches('/'))
                .filter(|d| !d.is_empty())
                .ok_or_else(|| AgentError::InvalidRequest("a directory is required".into()))?;
            if !Path::new(dir).is_dir() {
                return Err(AgentError::WorkspaceMissing(dir.to_string()));
            }
            let child =
                Arc::new(PiChild::spawn(&self.cmd, &self.launch(None, Path::new(dir))).await?);
            let created = async {
                if let Some(model) = model {
                    self.apply_model(&child, model).await?;
                }
                let state = child.request(json!({"type": "get_state"})).await?;
                let id = state
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| AgentError::Protocol("pi get_state has no sessionId".into()))?
                    .to_string();
                let file = state
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
                Ok::<_, AgentError>((id, file))
            }
            .await;
            let (id, file) = match created {
                Ok(created) => created,
                Err(error) => {
                    // Nothing is written before the first message, so a
                    // failed setup leaves no session behind.
                    child.close().await;
                    return Err(error);
                }
            };
            let asid = AgentSessionId(id.clone());
            self.adopt(
                &asid,
                child,
                LiveSession::new(&asid, file, dir.to_string(), 0),
            );
            let mut info = self
                .live_only_info(&id)
                .ok_or_else(|| AgentError::SessionNotFound(id.clone()))?;
            info.model = model.cloned();
            Ok(info)
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            match self.store.find(session_id).await {
                Ok(file) => self.info_for(&file).await,
                Err(AgentError::SessionNotFound(id)) => self
                    .live_only_info(session_id)
                    .ok_or(AgentError::SessionNotFound(id)),
                Err(error) => Err(error),
            }
        })
    }

    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::ByPath
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // Pi's tools read files off this host; paths travel in the text.
            let message = append_attachment_paths(text, attachments);
            let child = self.child_for(session_id).await?;
            let running = self.live.with(session_id, |s| s.running()) == Some(true);
            // An idle session always gets `prompt`: a `follow_up` sent to an
            // idle Pi waits for a run nobody starts.
            let command = match (running, delivery) {
                (true, Some("steer")) => json!({"type": "steer", "message": message}),
                (true, _) => {
                    json!({"type": "prompt", "message": message, "streamingBehavior": "followUp"})
                }
                (false, _) => json!({"type": "prompt", "message": message}),
            };
            match child.request(command).await {
                Ok(_) => Ok(()),
                // A run started under us (another client, a queued
                // follow-up). The refusal came before acceptance, so queueing
                // it behind the run is not a second submission.
                Err(AgentError::RequestFailed(reason))
                    if !running && reason.to_lowercase().contains("stream") =>
                {
                    child
                        .request(json!({"type": "prompt", "message": message, "streamingBehavior": "followUp"}))
                        .await
                        .map(|_| ())
                }
                Err(error) => Err(error),
            }
        })
    }

    fn revert_session<'a>(
        &'a self,
        _session_id: &'a str,
        _message_id: &'a str,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("revert".into())) })
    }

    fn interrupt<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // No child means no run: Pi only runs inside one.
            let Some(child) = self.host.get(session_id) else {
                return Ok(());
            };
            child.request(json!({"type": "abort"})).await.map(|_| ())
        })
    }

    fn switch_model<'a>(&'a self, session_id: &'a str, model: &'a ModelRef) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let child = self.child_for(session_id).await?;
            self.apply_model(&child, model).await
        })
    }

    fn switch_mode<'a>(&'a self, _session_id: &'a str, _mode: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("modes".into())) })
    }

    fn find_files<'a>(
        &'a self,
        _query: &'a str,
        _limit: usize,
        _directory: Option<&'a str>,
    ) -> AgentFuture<'a, Vec<Value>> {
        Box::pin(async { Err(AgentError::Unsupported("find_files".into())) })
    }

    fn reply_permission<'a>(
        &'a self,
        session_id: &'a str,
        request_id: &'a str,
        decision: PermissionDecision,
        message: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            // A `confirm` answer carries no text: a reason the user wrote
            // would be dropped on the floor, so it is refused.
            if message.is_some_and(|m| !m.trim().is_empty()) {
                return Err(AgentError::Unsupported("approval reason".into()));
            }
            let pending = self
                .live
                .with(session_id, |s| s.pending.iter().any(|p| p.id == request_id))
                .unwrap_or(false);
            if !pending {
                return Err(AgentError::InvalidRequest(
                    "that approval is no longer pending".into(),
                ));
            }
            let child = self
                .host
                .get(session_id)
                .ok_or_else(|| AgentError::NotAvailable("the pi session has exited".into()))?;
            // Pi has no "always" for a confirm; it allows this call only.
            let confirmed = !matches!(decision, PermissionDecision::Deny);
            let reply =
                json!({"type": "extension_ui_response", "id": request_id, "confirmed": confirmed});
            child.send(reply.clone()).await?;
            // Folded like a child record, so the mirror resolves it too.
            self.live
                .publish(&AgentSessionId(session_id.to_string()), reply);
            Ok(())
        })
    }

    fn reply_form<'a>(
        &'a self,
        _session_id: &'a str,
        _form_id: &'a str,
        _answers: Value,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async { Err(AgentError::Unsupported("forms".into())) })
    }

    fn get_catalog<'a>(&'a self, _directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            // Held across the read: concurrent callers share one probe child.
            let mut cached = self.catalog.lock().await;
            if let Some((at, catalog)) = cached.as_ref() {
                if at.elapsed() < CATALOG_TTL {
                    return Ok(catalog.clone());
                }
            }
            let catalog = self.read_catalog().await?;
            *cached = Some((Instant::now(), catalog.clone()));
            Ok(catalog)
        })
    }

    fn get_vcs_diff<'a>(
        &'a self,
        _session_id: &'a str,
        _mode: &'a str,
    ) -> AgentFuture<'a, Vec<FileDiffItem>> {
        Box::pin(async { Err(AgentError::Unsupported("vcs diff".into())) })
    }

    fn get_pending_permissions<'a>(
        &'a self,
        session_id: &'a str,
    ) -> AgentFuture<'a, Vec<PermissionRequest>> {
        Box::pin(async move {
            Ok(self
                .live
                .with(session_id, |s| s.pending.clone())
                .unwrap_or_default())
        })
    }

    fn get_pending_forms<'a>(&'a self, _session_id: &'a str) -> AgentFuture<'a, Vec<FormRequest>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn get_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let mut items = match self.store.find(session_id).await {
                Ok(file) => {
                    let entries = self.store.read_entries(&file).await?;
                    mapper::timeline_from_entries(&sessions::active_branch(&entries))
                }
                Err(AgentError::SessionNotFound(_)) if self.live.status(session_id).is_some() => {
                    Vec::new()
                }
                Err(error) => return Err(error),
            };
            // Rows the file already holds came from it; the rest are still
            // streaming (or not yet re-keyed) and only the fold has them.
            let live = self
                .live
                .with(session_id, |s| {
                    s.rows
                        .retain(|row| !items.iter().any(|item| item.id == row.id));
                    s.rows.clone()
                })
                .unwrap_or_default();
            items.extend(live);
            items.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
            let skip = items.len().saturating_sub(limit);
            Ok(items.split_off(skip))
        })
    }

    fn delete_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            if let Some(child) = self.host.remove(session_id) {
                child.close().await;
            }
            let was_live = self
                .live
                .sessions
                .lock()
                .map(|mut s| s.remove(session_id).is_some())
                .unwrap_or(false);
            match self.store.find(session_id).await {
                Ok(file) => self.store.delete(&file).await,
                // A fresh session never written to disk is gone with its child.
                Err(AgentError::SessionNotFound(_)) if was_live => Ok(()),
                Err(error) => Err(error),
            }
        })
    }

    fn rename_session<'a>(&'a self, session_id: &'a str, title: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let name = title.trim();
            if name.is_empty() {
                return Err(AgentError::InvalidRequest("a title is required".into()));
            }
            let child = self.child_for(session_id).await?;
            child
                .request(json!({"type": "set_session_name", "name": name}))
                .await
                .map(|_| ())
        })
    }

    fn compact_session<'a>(
        &'a self,
        session_id: &'a str,
        _delivery: Option<&'a str>,
    ) -> AgentFuture<'a, Value> {
        Box::pin(async move {
            let child = self.child_for(session_id).await?;
            // `compact` answers only when the summary is written, which can
            // outlast the request timeout; progress arrives as
            // `compaction_start`/`compaction_end` either way.
            let live = self.live.clone();
            let asid = AgentSessionId(session_id.to_string());
            tokio::spawn(async move {
                match child.request(json!({"type": "compact"})).await {
                    Ok(_) => {}
                    // Refused before it started: say so the way Pi would.
                    Err(AgentError::RequestFailed(reason)) => {
                        tracing::info!(%reason, "pi refused to compact");
                        live.publish(&asid, json!({"type": "compaction_end", "aborted": true}));
                    }
                    Err(error) => tracing::debug!(%error, "pi compact reply lost"),
                }
            });
            Ok(json!({"started": true}))
        })
    }
}
