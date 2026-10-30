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
//! Pi 1.4.0 sends no entry id for a live message, so whenever a message may
//! have reached the file (see [`rereads_tail`]) the forwarder reads the new
//! tail of the session file and feeds every new message entry back as a
//! synthetic `entry_appended`; the live folder re-keys its provisional
//! `live-<n>` rows to the real ids.
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
use super::rpc::{PiChild, PiCommandLine, PiHost, PiLaunch, PiLease, PiRecord};
use super::sessions::{self, SessionFile, SessionStore};
use crate::agents::domain::{
    AgentCatalog, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus, FormRequest,
    ModelRef, PermissionDecision, PermissionRequest, SessionContextUsage, SessionQuery,
    TimelineItem, TokensUsage,
};
use crate::agents::ports::agent::{
    AgentError, AgentFuture, AgentPort, AttachmentMode, FileDiffItem, HistoryCommit,
    PreparedTimeline,
};

/// Discovery asks for the catalog on every refresh; a probe child costs a
/// Node start-up, so the answer is reused this long.
const CATALOG_TTL: Duration = Duration::from_secs(60);
const COMMANDS_TTL: Duration = Duration::from_secs(30);
/// `pi --version` is a process too; health checks run every 15 s.
const PROBE_TTL: Duration = Duration::from_secs(60);
/// How often the reaper looks for idle children.
const REAP_TICK: Duration = Duration::from_secs(30);
/// How often a forwarder with nothing to forward checks its child is alive,
/// so a crash mid-run does not leave the session Busy forever.
const ALIVE_TICK: Duration = Duration::from_secs(2);
/// Overflow stops the children: a snapshot cannot recreate a lost approval.
const EVENT_CAPACITY: usize = 32;
/// Live rows kept per session for the timeline merge. Rows that reached the
/// file are dropped at the next read; this only bounds a session whose file
/// cannot be read.
const MAX_LIVE_ROWS: usize = 512;
const MAX_LIVE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TRACKED_SESSIONS: usize = 64;
const MAX_CHILDREN: usize = 8;
/// A synthetic record the driver publishes when a child replaces a dead one:
/// it names the dead child's open approvals and provisional rows so the
/// mirror drops them. The new child's folder never saw them, so this cannot
/// go through it. Pi has no record of this type.
const DISCARD: &str = "muqun_discard";

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
    model: Option<ModelRef>,
    tokens: Option<TokensUsage>,
    cost: Option<f64>,
    context_usage: Option<SessionContextUsage>,
    native_usage: bool,
    compacting: bool,
    failed: bool,
    /// Bytes of the file already seen, so the tail read is cheap.
    offset: u64,
    created_ms: u64,
    /// Which child this state belongs to, so the forwarder of a child that
    /// died cannot settle the run of the one that replaced it.
    generation: u64,
    /// Gateway-created blank identity, resumable through Pi's --session-id.
    /// Cleared permanently once a real user/assistant message is observed.
    provisional: bool,
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
            model: None,
            tokens: None,
            cost: None,
            context_usage: None,
            native_usage: false,
            compacting: false,
            failed: false,
            offset: 0,
            created_ms: now_ms(),
            generation: 0,
            provisional: false,
        }
    }

    fn running(&self) -> bool {
        self.compacting
            || matches!(
                self.status,
                AgentSessionStatus::Busy | AgentSessionStatus::Retry
            )
    }
}

/// State the forwarder and reaper tasks share with the driver.
struct Live {
    events: broadcast::Sender<(AgentSessionId, u64, Arc<PiRecord>)>,
    sessions: Mutex<HashMap<String, LiveSession>>,
    generations: std::sync::atomic::AtomicU64,
    store: SessionStore,
}

impl Live {
    fn publish(&self, asid: &AgentSessionId, record: Value) {
        // No subscriber yet (before the manager attaches) is not a failure.
        let generation = self.with(&asid.0, |s| s.generation).unwrap_or(0);
        let _ = self
            .events
            .send((asid.clone(), generation, PiRecord::cleanup(record)));
    }

    fn publish_child(&self, asid: &AgentSessionId, generation: u64, record: Arc<PiRecord>) {
        let _ = self.events.send((asid.clone(), generation, record));
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
    async fn read_tail(&self, asid: &str, generation: u64) -> Result<Vec<Value>, AgentError> {
        if self.with(asid, |s| s.generation == generation && s.file.is_none()) == Some(true) {
            if let Ok(file) = self.store.find(asid).await {
                self.with(asid, |s| {
                    if s.generation == generation && s.file.is_none() {
                        s.file = Some(file.path);
                    }
                });
            }
        }
        let Some(Some((Some(path), offset))) = self.with(asid, |s| {
            (s.generation == generation).then(|| (s.file.clone(), s.offset))
        }) else {
            return Ok(Vec::new());
        };
        let Ok(mut file) = sessions::open_regular(&path).await else {
            return Ok(Vec::new());
        };
        if file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        if let Err(error) = file
            .take((sessions::MAX_SESSION_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await
        {
            tracing::debug!(%error, "pi session tail unreadable");
            return Ok(Vec::new());
        }
        if bytes.len() > sessions::MAX_SESSION_BYTES {
            return Err(AgentError::Protocol(
                "pi session tail exceeds byte limit".into(),
            ));
        }
        if bytes
            .split(|b| *b == b'\n')
            .any(|line| line.len() > super::rpc::MAX_LINE_BYTES)
        {
            return Err(AgentError::Protocol(
                "pi session entry exceeds record limit".into(),
            ));
        }
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return Ok(Vec::new());
        };
        if self.with(asid, |s| {
            if s.generation != generation {
                return false;
            }
            s.offset = offset + end as u64 + 1;
            true
        }) != Some(true)
        {
            return Ok(Vec::new());
        }
        Ok(bytes[..end]
            .split(|b| *b == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
            .collect())
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
    spawn_lock: Arc<tokio::sync::Mutex<()>>,
    catalog: tokio::sync::Mutex<Option<(Instant, AgentCatalog)>>,
    commands_listed: std::sync::atomic::AtomicBool,
    command_cache: Mutex<HashMap<PathBuf, (Instant, Vec<crate::agents::domain::CommandInfo>)>>,
    probed: Mutex<Option<(Instant, bool)>>,
    failed: std::sync::atomic::AtomicBool,
    history: Mutex<HashMap<String, HistoryCursor>>,
    stop: CancellationToken,
}

#[derive(Clone)]
struct HistoryCursor {
    oldest: Option<String>,
    revision: u64,
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
            store: store.clone(),
            host: Arc::new(PiHost::new()),
            env,
            idle_timeout,
            live: Arc::new(Live {
                events,
                sessions: Mutex::new(HashMap::new()),
                generations: std::sync::atomic::AtomicU64::new(0),
                store,
            }),
            spawn_lock: Arc::new(tokio::sync::Mutex::new(())),
            catalog: tokio::sync::Mutex::new(None),
            commands_listed: std::sync::atomic::AtomicBool::new(false),
            command_cache: Mutex::new(HashMap::new()),
            probed: Mutex::new(None),
            failed: std::sync::atomic::AtomicBool::new(false),
            history: Mutex::new(HashMap::new()),
            stop: CancellationToken::new(),
        }
    }

    /// Every record any child prints, tagged with its session, plus the
    /// synthetic ones the driver adds (re-key entries, the replies it sent).
    pub fn subscribe(&self) -> broadcast::Receiver<(AgentSessionId, u64, Arc<PiRecord>)> {
        self.live.events.subscribe()
    }

    /// Cancelled by [`PiDriver::shutdown`], for tasks that must end with it.
    pub fn stopped(&self) -> CancellationToken {
        self.stop.clone()
    }

    pub fn is_connected(&self) -> bool {
        !self.stop.is_cancelled() && !self.failed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn ensure_open(&self) -> Result<(), AgentError> {
        if self.is_connected() {
            Ok(())
        } else {
            Err(AgentError::NotAvailable("pi runtime stopped".into()))
        }
    }

    pub async fn shutdown_and_wait(&self) {
        self.stop.cancel();
        let _one = self.spawn_lock.lock().await;
        self.host.shutdown_all().await;
    }

    pub(crate) fn has_native_usage(&self, session_id: &str) -> bool {
        self.live.with(session_id, |s| s.native_usage) == Some(true)
    }

    /// A lost event may be an approval or terminal state. Stop the runtime
    /// rather than pretending a file snapshot can reconstruct that decision.
    pub async fn abort_backlog(&self) -> Vec<(AgentSessionId, Vec<LiveChange>)> {
        self.failed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.stop.cancel();
        let _one = self.spawn_lock.lock().await;
        self.host.shutdown_all().await;
        let Ok(mut sessions) = self.live.sessions.lock() else {
            return Vec::new();
        };
        sessions
            .iter_mut()
            .map(|(id, session)| {
                let changes = session
                    .folder
                    .fold(&json!({"type":"agent_settled", "aborted":true}));
                for change in &changes {
                    apply(session, change);
                }
                session.failed = true;
                (AgentSessionId(id.clone()), changes)
            })
            .collect()
    }

    /// Start closing idle children, one [`reap_once`] pass per tick.
    pub fn start_reaper(&self) {
        let (host, live, stop, timeout, admission) = (
            self.host.clone(),
            self.live.clone(),
            self.stop.clone(),
            self.idle_timeout,
            self.spawn_lock.clone(),
        );
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_TICK);
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let _one = admission.lock().await;
                reap_once(&host, &live, timeout).await;
            }
        });
    }

    /// Kill every child. Called from the manager's synchronous shutdown hook,
    /// so the closing runs on a task.
    pub fn shutdown(&self) {
        self.stop.cancel();
        let host = self.host.clone();
        let admission = self.spawn_lock.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    let _one = admission.lock().await;
                    host.shutdown_all().await
                });
            }
            // No runtime left: dropping the host kills the children
            // (`kill_on_drop`).
            Err(_) => drop(host),
        }
    }

    /// Fold one record of `asid` into live state and return what changed.
    /// Dialogs that are not ours are answered `cancelled` here, so Pi never
    /// waits on a prompt nobody sees.
    ///
    /// A record for a session with no live state is dropped: `adopt` creates
    /// the state before it forwards anything, so such a record is one still
    /// in flight after `delete_session`, and folding it would bring the
    /// deleted session back as a ghost.
    pub async fn fold(
        &self,
        asid: &AgentSessionId,
        generation: u64,
        record: &Value,
    ) -> Vec<LiveChange> {
        let (changes, overflow) = {
            let Ok(mut sessions) = self.live.sessions.lock() else {
                return Vec::new();
            };
            let Some(session) = sessions.get_mut(&asid.0) else {
                return Vec::new();
            };
            if session.generation != generation {
                return Vec::new();
            }
            if session.failed {
                return Vec::new();
            }
            if matches!(
                record.get("type").and_then(Value::as_str),
                Some("message_start" | "message_end")
            ) && matches!(
                record.pointer("/message/role").and_then(Value::as_str),
                Some("user" | "assistant")
            ) {
                session.provisional = false;
            }
            let changes = if record.get("type").and_then(Value::as_str) == Some(DISCARD) {
                discarded(record)
            } else {
                session.folder.fold(record)
            };
            for change in &changes {
                apply(session, change);
            }
            let bytes = session
                .folder
                .retained_bytes()
                .saturating_add(
                    session
                        .rows
                        .iter()
                        .map(mapper::item_bytes)
                        .fold(0usize, usize::saturating_add),
                )
                .saturating_add(
                    session
                        .pending
                        .iter()
                        .map(mapper::permission_bytes)
                        .fold(0usize, usize::saturating_add),
                );
            if bytes > MAX_LIVE_BYTES {
                let mut cleanup = session
                    .folder
                    .fold(&json!({"type":"agent_settled", "aborted":true}));
                cleanup.push(LiveChange::Remove(
                    session.rows.iter().map(|r| r.id.clone()).collect(),
                ));
                cleanup.push(LiveChange::Status(AgentSessionStatus::Failed));
                session.folder = LiveFolder::new(asid.clone());
                session.rows.clear();
                session.pending.clear();
                session.failed = true;
                session.status = AgentSessionStatus::Failed;
                (cleanup, true)
            } else {
                (changes, false)
            }
        };
        if overflow {
            if let Some(child) = self.host.get(&asid.0) {
                child.terminate("pi live state exceeds byte limit");
            }
        }
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
            session_id: None,
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
        let replaced = self
            .live
            .sessions
            .lock()
            .ok()
            .and_then(|mut sessions| sessions.insert(asid.0.clone(), session));
        // A previous child's dialogs and unfinished rows died with it. Its
        // forwarder stays silent (the generation moved on), so they are
        // dropped here; published before the new forwarder starts, the
        // discard folds ahead of anything the new child says.
        if let Some(old) = replaced {
            let permissions: Vec<&str> = old.pending.iter().map(|p| p.id.as_str()).collect();
            let rows: Vec<&str> = old
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .filter(|id| mapper::is_provisional(id))
                .collect();
            if !permissions.is_empty() || !rows.is_empty() {
                self.live.publish(
                    asid,
                    json!({"type": DISCARD, "permissions": permissions, "rows": rows}),
                );
            }
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
    async fn child_for(&self, session_id: &str) -> Result<PiLease, AgentError> {
        self.ensure_open()?;
        if let Some(child) = self.host.lease(session_id) {
            return Ok(child);
        }
        let _one = self.spawn_lock.lock().await;
        self.ensure_open()?;
        if let Some(child) = self.host.lease(session_id) {
            return Ok(child);
        }
        self.reserve_slot(Some(session_id))?;
        let file = match self.store.find(session_id).await {
            Ok(file) => file,
            Err(AgentError::SessionNotFound(_))
                if self.live.with(session_id, |s| s.provisional) == Some(true) =>
            {
                return self.resume_blank(session_id).await;
            }
            Err(error) => return Err(error),
        };
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
        if let Err(error) = self.ensure_open() {
            child.close().await;
            return Err(error);
        }
        let resumed = child.request(json!({"type":"get_state"})).await;
        match resumed {
            Ok(state) if state.get("sessionId").and_then(Value::as_str) == Some(session_id) => {}
            Ok(_) => {
                child.close().await;
                return Err(AgentError::Protocol(
                    "pi resumed a different session".into(),
                ));
            }
            Err(error) => {
                child.close().await;
                return Err(error);
            }
        }
        let asid = AgentSessionId(file.id.clone());
        let mut session = LiveSession::new(&asid, Some(file.path.clone()), file.cwd, history_len);
        session.folder = session
            .folder
            .with_history_usage(&sessions::active_branch(&entries));
        session.offset = offset;
        session.created_ms = file.created_ms;
        self.adopt(&asid, child.clone(), session);
        tracing::debug!(session = %asid.0, "started a pi child");
        child
            .lease()
            .ok_or_else(|| AgentError::NotAvailable("pi session exited during startup".into()))
    }

    /// Pi deliberately writes no setup-only file. Preserve only the bounded
    /// metadata we already own and ask its native CLI to recreate that ID.
    async fn resume_blank(&self, id: &str) -> Result<PiLease, AgentError> {
        let (cwd, model, title, created_ms) = self
            .live
            .with(id, |s| {
                (
                    s.cwd.clone(),
                    s.model.clone(),
                    s.title.clone(),
                    s.created_ms,
                )
            })
            .ok_or_else(|| AgentError::SessionNotFound(id.into()))?;
        if !Path::new(&cwd).is_dir() {
            return Err(AgentError::WorkspaceMissing(cwd));
        }
        let mut launch = self.launch(None, Path::new(&cwd));
        launch.session_id = Some(id.into());
        let child = Arc::new(PiChild::spawn(&self.cmd, &launch).await?);
        let restored = async {
            let state = child.request(json!({"type":"get_state"})).await?;
            if state.get("sessionId").and_then(Value::as_str) != Some(id) {
                return Err(AgentError::Protocol(
                    "pi recreated a different session".into(),
                ));
            }
            if let Some(model) = &model {
                self.apply_model(&child, model).await?;
            }
            if let Some(title) = &title {
                child
                    .request(json!({"type":"set_session_name", "name":title}))
                    .await?;
            }
            self.ensure_open()?;
            Ok(state)
        }
        .await;
        let state = match restored {
            Ok(state) => state,
            Err(error) => {
                child.close().await;
                return Err(error);
            }
        };
        let asid = AgentSessionId(id.into());
        let file = state
            .get("sessionFile")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        let mut session = LiveSession::new(&asid, file, cwd, 0);
        session.model = model;
        session.title = title;
        session.created_ms = created_ms;
        session.provisional = true;
        self.adopt(&asid, child.clone(), session);
        child
            .lease()
            .ok_or_else(|| AgentError::NotAvailable("pi session exited during startup".into()))
    }

    /// Eviction drops only cached state whose child is gone. Session files
    /// and their workspace data remain authoritative and untouched.
    fn reserve_slot(&self, existing: Option<&str>) -> Result<(), AgentError> {
        if self.host.alive_count() >= MAX_CHILDREN {
            return Err(AgentError::NotAvailable(
                "pi live child limit reached".into(),
            ));
        }
        let mut sessions = self
            .live
            .sessions
            .lock()
            .map_err(|_| AgentError::NotAvailable("pi state unavailable".into()))?;
        if existing.is_some_and(|id| sessions.contains_key(id)) {
            return Ok(());
        }
        if sessions.len() >= MAX_TRACKED_SESSIONS {
            let evict = sessions
                .iter()
                .filter(|(id, s)| !s.provisional && self.host.get(id).is_none())
                .min_by_key(|(_, s)| s.generation)
                .map(|(id, _)| id.clone());
            if let Some(id) = evict {
                sessions.remove(&id);
            }
        }
        if sessions.len() >= MAX_TRACKED_SESSIONS {
            return Err(AgentError::NotAvailable(
                "pi live session limit reached".into(),
            ));
        }
        Ok(())
    }

    fn prepare_history<'a>(
        &'a self,
        session_id: &'a str,
        items: Vec<TimelineItem>,
        previous: Option<HistoryCursor>,
        oldest: Option<String>,
    ) -> PreparedTimeline<'a> {
        PreparedTimeline {
            items,
            commit: Some(Box::new(move || {
                let mut cursors = self
                    .history
                    .lock()
                    .map_err(|_| AgentError::HistoryCursorLost)?;
                if cursors.get(session_id).map(|c| c.revision)
                    != previous.as_ref().map(|c| c.revision)
                {
                    return Err(AgentError::HistoryCursorLost);
                }
                if cursors.len() >= MAX_TRACKED_SESSIONS && !cursors.contains_key(session_id) {
                    if let Some(evict) = cursors.keys().next().cloned() {
                        cursors.remove(&evict);
                    }
                }
                cursors.insert(
                    session_id.to_string(),
                    HistoryCursor {
                        oldest,
                        revision: previous.map_or(1, |c| c.revision.wrapping_add(1)),
                    },
                );
                Ok(HistoryCommit::default())
            })),
        }
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
            info.model = s.model.clone();
            info.tokens = s.tokens.clone();
            info.cost = s.cost;
            info.context_usage = s.context_usage.clone();
            info
        })
    }

    fn status_of(&self, session_id: &str) -> AgentSessionStatus {
        self.live
            .status(session_id)
            .unwrap_or(AgentSessionStatus::Idle)
    }

    async fn info_for(&self, file: &SessionFile) -> Result<AgentSessionInfo, AgentError> {
        let mut info = self
            .store
            .read_summary(file, self.status_of(&file.id))
            .await?;
        if self.host.get(&file.id).is_some() {
            self.live.with(&file.id, |s| {
                if s.model.is_some() {
                    info.model = s.model.clone();
                }
                if s.tokens.is_some() {
                    info.tokens = s.tokens.clone();
                }
                if s.cost.is_some() {
                    info.cost = s.cost;
                }
            });
        }
        self.refresh_stats(&mut info).await;
        Ok(info)
    }

    async fn refresh_stats(&self, info: &mut AgentSessionInfo) {
        let Some(child) = self.host.get(&info.asid.0) else {
            return;
        };
        let generation = self.live.with(&info.asid.0, |s| s.generation);
        if let Ok(stats) = child.request(json!({"type":"get_session_stats"})).await {
            if self.live.with(&info.asid.0, |s| s.generation) != generation {
                return;
            }
            mapper::apply_stats(info, &stats);
            self.live.with(&info.asid.0, |s| {
                s.tokens = info.tokens.clone();
                s.cost = info.cost;
                s.context_usage = info.context_usage.clone();
                s.native_usage = stats.get("tokens").is_some();
            });
        }
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
        let _one = self.spawn_lock.lock().await;
        self.ensure_open()?;
        let cwd = dirs::home_dir().unwrap_or_else(std::env::temp_dir);
        let launch = PiLaunch {
            session_file: None,
            session_id: None,
            cwd,
            env: Vec::new(),
            no_extensions: true,
        };
        let child = Arc::new(PiChild::spawn(&self.cmd, &launch).await?);
        let mut owned = self.host.hold_probe(child.clone());
        let operation = async {
            let models = child
                .request(json!({"type": "get_available_models"}))
                .await?;
            let levels = child
                .request(json!({"type": "get_available_thinking_levels"}))
                .await?;
            let state = child.request(json!({"type": "get_state"})).await?;
            Ok::<_, AgentError>(mapper::catalog(&models, &levels, state.get("model")))
        };
        let read = tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(AgentError::NotAvailable("pi runtime stopped".into())),
            read = operation => read,
        };
        child.close().await;
        owned.finish();
        read
    }

    async fn read_commands(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<crate::agents::domain::CommandInfo>, AgentError> {
        let current = self.live.sessions.lock().ok().and_then(|sessions| {
            sessions.iter().find_map(|(id, s)| {
                directory
                    .filter(|d| *d == s.cwd)
                    .and_then(|_| self.host.get(id))
            })
        });
        if let Some(child) = current {
            let value = child.request(json!({"type":"get_commands"})).await?;
            self.commands_listed
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Ok(mapper::commands(&value));
        }
        let cwd = directory
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        if !cwd.is_dir() {
            return Err(AgentError::WorkspaceMissing(
                cwd.to_string_lossy().into_owned(),
            ));
        }
        if let Some(commands) = self.command_cache.lock().ok().and_then(|cache| {
            cache
                .get(&cwd)
                .filter(|(at, _)| at.elapsed() < COMMANDS_TTL)
                .map(|(_, commands)| commands.clone())
        }) {
            return Ok(commands);
        }
        let _one = self.spawn_lock.lock().await;
        self.ensure_open()?;
        let child = Arc::new(
            PiChild::spawn(
                &self.cmd,
                &PiLaunch {
                    session_file: None,
                    session_id: None,
                    cwd: cwd.clone(),
                    env: Vec::new(),
                    no_extensions: true,
                },
            )
            .await?,
        );
        let mut owned = self.host.hold_probe(child.clone());
        let result = tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(AgentError::NotAvailable("pi runtime stopped".into())),
            result = child.request(json!({"type":"get_commands"})) => result,
        };
        child.close().await;
        owned.finish();
        let result = result?;
        self.commands_listed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let commands = mapper::commands(&result);
        if let Ok(mut cache) = self.command_cache.lock() {
            if cache.len() >= MAX_TRACKED_SESSIONS && !cache.contains_key(&cwd) {
                if let Some(directory) = cache.keys().next().cloned() {
                    cache.remove(&directory);
                }
            }
            cache.insert(cwd, (Instant::now(), commands.clone()));
        }
        Ok(commands)
    }
}

/// Keep the driver's view of a session in step with one fold result.
fn apply(session: &mut LiveSession, change: &LiveChange) {
    match change {
        LiveChange::Model(model) => session.model = Some(model.clone()),
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
        LiveChange::Usage { tokens, cost, .. } => {
            if !session.native_usage {
                session.tokens = Some(tokens.clone());
                session.cost = *cost;
            }
        }
        LiveChange::Compaction { status, .. } => {
            session.compacting = matches!(
                status,
                crate::agents::domain::CompactionStatus::Started
                    | crate::agents::domain::CompactionStatus::Running
            )
        }
        LiveChange::UiCancel(_) => {}
    }
}

/// The changes a [`DISCARD`] record stands for.
fn discarded(record: &Value) -> Vec<LiveChange> {
    let ids = |key: &str| -> Vec<String> {
        record
            .get(key)
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut changes: Vec<LiveChange> = ids("permissions")
        .into_iter()
        .map(LiveChange::PermissionResolved)
        .collect();
    let rows = ids("rows");
    if !rows.is_empty() {
        changes.push(LiveChange::Remove(rows));
    }
    changes
}

/// One reaper pass: close children idle past `timeout`.
///
/// Preserve active work, approvals and unpersisted rows. A discoverable
/// native file is also required: before Pi writes its first message the
/// child is the only owner of the session identity. The host atomically
/// excludes operations and native work from retirement admission.
async fn reap_once(host: &PiHost, live: &Live, timeout: Duration) {
    // Native activity during file validation invalidates this pass. Recheck
    // its stamp atomically with retirement rather than using a stale snapshot.
    let observed = host.activity_stamps();
    struct Candidate {
        asid: String,
        protected: bool,
        file: Option<PathBuf>,
        generation: u64,
        provisional: bool,
    }
    let Ok(candidates) = live.sessions.lock().map(|sessions| {
        sessions
            .iter()
            .map(|(asid, s)| Candidate {
                asid: asid.clone(),
                protected: s.running()
                    || !s.pending.is_empty()
                    || s.rows.iter().any(|r| mapper::is_provisional(&r.id)),
                file: s.file.clone(),
                generation: s.generation,
                provisional: s.provisional,
            })
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    let mut keep = std::collections::HashSet::new();
    let mut blanks = std::collections::HashSet::new();
    for candidate in &candidates {
        let child = host.get(&candidate.asid);
        if child
            .as_ref()
            .is_some_and(|child| child.idle_for() < timeout)
        {
            keep.insert(candidate.asid.clone());
            continue;
        }
        // Resume uses native discovery. An absent file is allowed only for
        // our own genuinely blank identity, recreated via --session-id.
        let unwritten = match &candidate.file {
            None => true,
            Some(path) => live
                .store
                .find(&candidate.asid)
                .await
                .map_or(true, |f| f.path != *path),
        };
        let blank = candidate.provisional
            && child.as_ref().is_some_and(|c| !c.has_conversation())
            && candidate.file.as_ref().is_none_or(|p| !p.exists());
        if candidate.protected || (unwritten && !blank) {
            keep.insert(candidate.asid.clone());
        } else if unwritten && blank {
            blanks.insert(candidate.asid.clone());
        }
    }
    for asid in host.reap_idle(timeout, &keep, Some(&observed)).await {
        if let Ok(mut current) = live.sessions.lock() {
            let generation = candidates
                .iter()
                .find(|c| c.asid == asid)
                .map(|c| c.generation);
            if current.get(&asid).map(|s| s.generation) == generation {
                if blanks.contains(&asid) {
                    // Keep only the bounded blank descriptor, never an
                    // unpersisted conversation or a retained process.
                    if let Some(session) = current.get_mut(&asid) {
                        session.folder = LiveFolder::new(AgentSessionId(asid.clone()));
                        session.rows.clear();
                    }
                } else {
                    current.remove(&asid);
                }
            }
        }
        tracing::debug!(%asid, "closed an idle pi child");
    }
}

/// Records after which a message entry may have reached the session file.
///
/// Pi prints `message_end` and then appends the entry, so the read on
/// `message_end` usually sees it but can lose that race. The records that
/// follow before any wait (`tool_execution_start`, an approval request)
/// catch it before the user looks; `turn_end` and `agent_settled` catch
/// whatever is left. Without the early reads a tool run or an approval wait
/// shows each assistant message twice: once from the file and once as its
/// not yet re-keyed live row.
fn rereads_tail(record: &Value) -> bool {
    matches!(
        record.get("type").and_then(Value::as_str),
        Some(
            "message_end"
                | "tool_execution_start"
                | "extension_ui_request"
                | "turn_end"
                | "agent_settled"
        )
    )
}

/// Forward one child's records until it exits. Whenever a message may have
/// been persisted, the file's new message entries follow as `entry_appended`
/// so live rows are re-keyed. A child that dies leaves no run and no dialog
/// behind.
async fn forward(
    live: Arc<Live>,
    asid: AgentSessionId,
    generation: u64,
    child: Arc<PiChild>,
    mut events: broadcast::Receiver<Arc<PiRecord>>,
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
                if live.with(&asid.0, |s| s.generation) != Some(generation) {
                    return;
                }
                let reread = rereads_tail(&record);
                live.publish_child(&asid, generation, record);
                if reread {
                    let entries = match live.read_tail(&asid.0, generation).await {
                        Ok(entries) => entries,
                        Err(_) => {
                            child.terminate("pi session tail exceeds byte limit");
                            break;
                        }
                    };
                    for entry in entries {
                        let mut record = json!({"type": "entry_appended", "entry": entry});
                        super::rpc::omit_user_image_echo(&mut record);
                        let bytes = serde_json::to_vec(&record).map_or(usize::MAX, |b| b.len());
                        let Some(record) = PiRecord::bounded(record, bytes) else {
                            child.terminate("pi event byte budget exceeded");
                            break;
                        };
                        live.publish_child(&asid, generation, record);
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(session = %asid.0, skipped, "pi events lagged");
                child.terminate("pi event backlog overflowed");
                break;
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
        live.publish_child(
            &asid,
            generation,
            PiRecord::cleanup(
                json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
            ),
        );
    }
    if running {
        tracing::warn!(session = %asid.0, "pi exited during a run");
        live.publish_child(
            &asid,
            generation,
            PiRecord::cleanup(json!({"type": "agent_settled", "aborted": true})),
        );
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn model_from_state(state: &Value) -> Option<ModelRef> {
    Some(ModelRef {
        provider_id: state.pointer("/model/provider")?.as_str()?.to_string(),
        model_id: state.pointer("/model/id")?.as_str()?.to_string(),
        variant: state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

impl AgentPort for PiDriver {
    fn kind(&self) -> &'static str {
        super::KIND
    }

    fn supports_earlier_timeline(&self) -> bool {
        true
    }

    fn slash_commands_listed(&self) -> bool {
        self.commands_listed
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Healthy while any child is alive: a failed probe makes the runtime
    /// shut the manager down, which kills every child, and a `pi --version`
    /// that fails once (a loaded host, an upgrade swapping the install) says
    /// nothing about the children already running. Only with none alive
    /// does the binary's answer decide.
    fn probe(&self) -> AgentFuture<'_, bool> {
        Box::pin(async move {
            if !self.is_connected() {
                return Ok(false);
            }
            if self.host.any_alive() {
                return Ok(true);
            }
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
                                && (s.provisional || self.host.get(id).is_some())
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
            self.ensure_open()?;
            let _one = self.spawn_lock.lock().await;
            self.ensure_open()?;
            self.reserve_slot(None)?;
            let dir = directory
                .map(|d| d.trim().trim_end_matches('/'))
                .filter(|d| !d.is_empty())
                .ok_or_else(|| AgentError::InvalidRequest("a directory is required".into()))?;
            if !Path::new(dir).is_dir() {
                return Err(AgentError::WorkspaceMissing(dir.to_string()));
            }
            let child =
                Arc::new(PiChild::spawn(&self.cmd, &self.launch(None, Path::new(dir))).await?);
            let setup = async {
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
                let model = model_from_state(&state);
                Ok::<_, AgentError>((id, file, model))
            };
            let created = tokio::select! {
                biased;
                _ = self.stop.cancelled() => Err(AgentError::NotAvailable("pi runtime stopped".into())),
                result = setup => result,
            };
            let (id, file, effective_model) = match created {
                Ok(created) => created,
                Err(error) => {
                    // Nothing is written before the first message, so a
                    // failed setup leaves no session behind.
                    child.close().await;
                    return Err(error);
                }
            };
            let asid = AgentSessionId(id.clone());
            if let Err(error) = self.ensure_open() {
                child.close().await;
                return Err(error);
            }
            let mut session = LiveSession::new(&asid, file, dir.to_string(), 0);
            session.provisional = true;
            session.model = effective_model;
            self.adopt(&asid, child, session);
            let info = self
                .live_only_info(&id)
                .ok_or_else(|| AgentError::SessionNotFound(id.clone()))?;
            Ok(info)
        })
    }

    fn get_session<'a>(&'a self, session_id: &'a str) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            match self.store.find(session_id).await {
                Ok(file) => self.info_for(&file).await,
                Err(AgentError::SessionNotFound(id)) => {
                    let mut info = self
                        .live_only_info(session_id)
                        .ok_or(AgentError::SessionNotFound(id))?;
                    self.refresh_stats(&mut info).await;
                    Ok(info)
                }
                Err(error) => Err(error),
            }
        })
    }

    fn attachment_mode(&self) -> AttachmentMode {
        AttachmentMode::NativeUploads
    }

    fn prepare_selected_session<'a>(&'a self, id: &'a str) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let child = self.child_for(id).await?;
            let state = child.request(json!({"type":"get_state"})).await?;
            if state.get("sessionId").and_then(Value::as_str) != Some(id) {
                return Err(AgentError::Protocol(
                    "pi resumed a different session".into(),
                ));
            }
            Ok(())
        })
    }

    fn prepare_observed_session<'a>(&'a self, _id: &'a str) -> AgentFuture<'a, ()> {
        // Snapshots and polls read native files without waking a process.
        Box::pin(async { Ok(()) })
    }

    fn send_prompt<'a>(
        &'a self,
        session_id: &'a str,
        text: &'a str,
        attachments: &'a [String],
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let prepared = super::prompt::prepare(text, attachments).await?;
            // Validate the longest delivery envelope before starting a child.
            let mut command = prepared.command("prompt", Some("followUp"))?;
            let child = self.child_for(session_id).await?;
            let running = self.live.with(session_id, |s| s.running()) == Some(true);
            // An idle session always gets `prompt`: a `follow_up` sent to an
            // idle Pi waits for a run nobody starts.
            if !running || delivery == Some("steer") {
                command
                    .as_object_mut()
                    .expect("Pi prompt is an object")
                    .remove("streamingBehavior");
            }
            if running && delivery == Some("steer") {
                command["type"] = json!("steer");
            }
            self.ensure_open()?;
            // A rejected, timed-out or disconnected submission is never
            // automatically replayed: its delivery may be ambiguous.
            child.request(command).await.map(|_| ())
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
            let applied = self.apply_model(&child, model).await;
            // set_model may succeed before set_thinking_level fails. Read
            // the actual state even then, without retrying either mutation.
            match child.request(json!({"type":"get_state"})).await {
                Ok(state) => {
                    let effective = model_from_state(&state);
                    self.live.with(session_id, |s| s.model = effective.clone());
                    if let Some(model) = effective {
                        self.live.publish(
                            &AgentSessionId(session_id.to_string()),
                            json!({"type":"muqun_model","model":model}),
                        );
                    }
                }
                Err(error) if applied.is_ok() => return Err(error),
                Err(_) => {}
            }
            applied
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
            if decision == PermissionDecision::AllowAlways {
                return Err(AgentError::Unsupported("persistent tool approval".into()));
            }
            let generation = self
                .live
                .with(session_id, |s| {
                    let at = s.pending.iter().position(|p| p.id == request_id)?;
                    s.pending.remove(at);
                    Some(s.generation)
                })
                .flatten();
            let Some(generation) = generation else {
                return Err(AgentError::InvalidRequest(
                    "that approval is no longer pending".into(),
                ));
            };
            let child = self
                .host
                .get(session_id)
                .ok_or_else(|| AgentError::NotAvailable("the pi session has exited".into()))?;
            if self.live.with(session_id, |s| s.generation) != Some(generation) {
                return Err(AgentError::InvalidRequest(
                    "that approval is no longer pending".into(),
                ));
            }
            let confirmed = !matches!(decision, PermissionDecision::Deny);
            let reply =
                json!({"type": "extension_ui_response", "id": request_id, "confirmed": confirmed});
            child.send(reply.clone()).await?;
            // Folded like a child record, so the mirror resolves it too.
            self.live.publish_child(
                &AgentSessionId(session_id.to_string()),
                generation,
                PiRecord::cleanup(reply),
            );
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

    fn get_catalog<'a>(&'a self, directory: Option<&'a str>) -> AgentFuture<'a, AgentCatalog> {
        Box::pin(async move {
            // Held across the read: concurrent callers share one probe child.
            let mut catalog = {
                let mut cached = self.catalog.lock().await;
                if let Some((at, catalog)) =
                    cached.as_ref().filter(|(at, _)| at.elapsed() < CATALOG_TTL)
                {
                    let _ = at;
                    catalog.clone()
                } else {
                    let catalog = self.read_catalog().await?;
                    *cached = Some((Instant::now(), catalog.clone()));
                    catalog
                }
            };
            catalog.commands = self.read_commands(directory).await?;
            Ok(catalog)
        })
    }

    fn run_command<'a>(
        &'a self,
        session_id: &'a str,
        name: &'a str,
        arguments: &'a str,
        delivery: Option<&'a str>,
    ) -> AgentFuture<'a, ()> {
        Box::pin(async move {
            let child = self.child_for(session_id).await?;
            let listed = child.request(json!({"type":"get_commands"})).await?;
            if !mapper::commands(&listed)
                .iter()
                .any(|command| command.name == name)
            {
                return Err(AgentError::InvalidRequest(
                    "that Pi command is unavailable in this session".into(),
                ));
            }
            let message = if arguments.is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {arguments}")
            };
            let mut command = json!({"type":"prompt", "message":message});
            if self.live.with(session_id, |s| s.running()) == Some(true) {
                command["streamingBehavior"] = json!(if delivery == Some("steer") {
                    "steer"
                } else {
                    "followUp"
                });
            }
            // Extension commands are accepted only through prompt, including
            // while busy; the separate steer RPC rejects extension commands.
            child.request(command).await.map(|_| ())
        })
    }

    fn fork_session<'a>(
        &'a self,
        session_id: &'a str,
        message_id: Option<&'a str>,
    ) -> AgentFuture<'a, AgentSessionInfo> {
        Box::pin(async move {
            if message_id.is_some() {
                return Err(AgentError::Unsupported("fork at a message boundary".into()));
            }
            let _one = self.spawn_lock.lock().await;
            self.ensure_open()?;
            self.reserve_slot(None)?;
            super::fork::clone_session(&self.cmd, &self.store, &self.host, &self.stop, session_id)
                .await
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

    fn prepare_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            let previous = self
                .history
                .lock()
                .map_err(|_| AgentError::HistoryCursorLost)?
                .get(session_id)
                .cloned();
            let mut items = self.get_timeline(session_id, usize::MAX).await?;
            let retained = previous
                .as_ref()
                .and_then(|c| c.oldest.as_ref())
                .filter(|id| items.iter().any(|r| &r.id == *id))
                .cloned();
            let start = items.len().saturating_sub(limit);
            let tail = items.split_off(start);
            let oldest = retained.or_else(|| tail.first().map(|r| r.id.clone()));
            Ok(self.prepare_history(session_id, tail, previous, oldest))
        })
    }

    fn prepare_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, PreparedTimeline<'a>> {
        Box::pin(async move {
            let previous = self
                .history
                .lock()
                .map_err(|_| AgentError::HistoryCursorLost)?
                .get(session_id)
                .cloned()
                .ok_or(AgentError::HistoryCursorLost)?;
            let items = self.get_timeline(session_id, usize::MAX).await?;
            let end = match &previous.oldest {
                Some(id) => items
                    .iter()
                    .position(|r| &r.id == id)
                    .ok_or(AgentError::HistoryCursorLost)?,
                None if items.is_empty() => 0,
                None => return Err(AgentError::HistoryCursorLost),
            };
            let start = end.saturating_sub(limit);
            let page = items[start..end].to_vec();
            let oldest = page
                .first()
                .map(|r| r.id.clone())
                .or_else(|| previous.oldest.clone());
            Ok(self.prepare_history(session_id, page, Some(previous), oldest))
        })
    }

    fn load_earlier_timeline<'a>(
        &'a self,
        session_id: &'a str,
        limit: usize,
    ) -> AgentFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let mut prepared = self.prepare_earlier_timeline(session_id, limit).await?;
            if let Some(commit) = prepared.commit.take() {
                commit()?;
            }
            Ok(prepared.items)
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
            if let Ok(mut cursors) = self.history.lock() {
                cursors.remove(session_id);
            }
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
            tokio::spawn(async move {
                match child.request(json!({"type": "compact"})).await {
                    Ok(_) => {}
                    // Pi emits its own terminal compaction event before a
                    // failed response. Do not overwrite that precise failure
                    // with a fabricated generic aborted event.
                    Err(AgentError::RequestFailed(reason)) => {
                        tracing::info!(%reason, "pi refused to compact");
                    }
                    Err(error) => tracing::debug!(%error, "pi compact reply lost"),
                }
            });
            Ok(json!({"started": true}))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Pi stand-in: `--version` fails, `get_state` answers a fresh session
    /// whose file would be `<cwd>/session.jsonl`, anything else succeeds.
    const FAKE_PI: &str = r#"#!/usr/bin/env python3
import json, os, sys, uuid
if "--version" in sys.argv:
    sys.exit(1)
sid = sys.argv[sys.argv.index("--session-id") + 1] if "--session-id" in sys.argv else str(uuid.uuid4())
model = {"provider": "example", "id": "default"}
level = "medium"
session_file = os.path.join(os.environ.get("PI_CODING_AGENT_SESSION_DIR", os.getcwd()), "2026-10-14T06-51-18-225Z_" + sid + ".jsonl")
if "--session" in sys.argv:
    loaded = sys.argv[sys.argv.index("--session") + 1]
    session_file = loaded
    with open(loaded) as f:
        sid = json.loads(f.readline())["id"]
    with open(loaded, "a") as f:
        f.write(json.dumps({"type":"model_change","id":"initialized","parentId":"entry","provider":"example","modelId":"default"}) + "\n")
for raw in sys.stdin.buffer:
    cmd = json.loads(raw)
    if cmd.get("type") == "set_model":
        model = {"provider": cmd.get("provider"), "id": cmd.get("modelId")}
    if cmd.get("type") == "set_thinking_level" and not os.path.exists("reject-thinking"):
        level = "high" if cmd.get("level") == "xhigh" else cmd.get("level")
    if cmd.get("type") == "clone":
        source = sys.argv[sys.argv.index("--session") + 1]
        with open(source) as f:
            entries = [json.loads(line) for line in f]
        sid = str(uuid.uuid4())
        entries[0]["id"] = sid
        entries[0]["parentSession"] = source
        session_file = os.path.join(os.environ.get("PI_CODING_AGENT_SESSION_DIR", os.path.dirname(source)), "2026-10-14T06-51-18-225Z_" + sid + ".jsonl")
        with open(session_file, "w") as f:
            f.writelines(json.dumps(entry) + "\n" for entry in entries)
    data = {"sessionId": sid, "sessionFile": session_file, "model": model, "thinkingLevel": level}
    if cmd.get("type") == "clone":
        data = {"cancelled": False}
    if cmd.get("type") == "get_commands":
        data = {"commands": [{"name": "example", "source": "extension"}, {"name": "skill:review", "source": "skill"}]}
    with open("commands.jsonl", "a") as log:
        log.write(json.dumps(cmd) + "\n")
    if cmd.get("type") == "prompt" and os.path.exists("reject-prompt"):
        result = {"success": False, "error": "stream failed after submission; private-token"}
    elif cmd.get("type") == "set_thinking_level" and os.path.exists("reject-thinking"):
        result = {"success": False, "error": "level rejected; private-token"}
    else:
        result = {"success": True, "data": data}
    sys.stdout.write(json.dumps({"id": cmd.get("id"), "type": "response",
                                 "command": cmd.get("type"), **result}) + "\n")
    sys.stdout.flush()
"#;

    /// A driver over the stand-in and a scratch directory, or `None` when
    /// the host has no python3.
    fn fake_driver(idle_timeout: Duration) -> Option<(PiDriver, PathBuf)> {
        let python = std::process::Command::new("python3")
            .arg("--version")
            .output();
        if !python.is_ok_and(|out| out.status.success()) {
            eprintln!("python3 missing; skipping");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("muqun-pi-driver-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        let binary = dir.join("pi");
        std::fs::write(&binary, FAKE_PI).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cmd = PiCommandLine {
            binary,
            extension: dir.join("muqun-extension.js"),
        };
        let store = SessionStore::new(dir.join("sessions"));
        Some((
            PiDriver::new(cmd, store, true, idle_timeout, Vec::new()),
            dir,
        ))
    }

    /// Only known blank identities may retire before native persistence.
    /// Once conversation starts, an absent file must retain the process.
    #[tokio::test]
    async fn reaper_retires_blank_and_written_but_keeps_unpersisted_conversation() {
        let Some((driver, dir)) = fake_driver(Duration::ZERO) else {
            return;
        };
        for (state, kept) in [
            ("blank", false),
            ("unpersisted", true),
            ("written", false),
            ("approval", true),
            ("row", true),
            ("running", true),
        ] {
            let project = dir.join(format!("project-{state}"));
            std::fs::create_dir_all(&project).unwrap();
            let info = driver
                .create_session(project.to_str(), None, None)
                .await
                .unwrap();
            if state != "blank" {
                driver.live.with(&info.asid.0, |s| s.provisional = false);
            }
            if !matches!(state, "blank" | "unpersisted") {
                let path = dir
                    .join("sessions")
                    .join(format!("2026-10-14T06-51-18-225Z_{}.jsonl", info.asid.0));
                std::fs::write(
                    &path,
                    format!(
                        "{}\n",
                        json!({"type":"session", "id":info.asid.0, "cwd":project})
                    ),
                )
                .unwrap();
                driver.live.with(&info.asid.0, |s| s.file = Some(path));
            }
            let record = match state {
                "approval" => Some(
                    json!({"type":"extension_ui_request", "id":"keep", "method":"confirm", "title":"muqun.permission", "message": json!({"tool":"bash", "input":{"command":"true"}}).to_string()}),
                ),
                "row" => Some(
                    json!({"type":"message_end", "message":{"role":"assistant", "content":[{"type":"text", "text":"not saved yet"}], "stopReason":"stop"}}),
                ),
                "running" => Some(json!({"type":"agent_start"})),
                _ => None,
            };
            if let Some(record) = record {
                let generation = driver.live.with(&info.asid.0, |s| s.generation).unwrap();
                driver.fold(&info.asid, generation, &record).await;
            }
            reap_once(&driver.host, &driver.live, Duration::ZERO).await;
            assert_eq!(
                driver.host.get(&info.asid.0).is_some(),
                kept,
                "state={state}"
            );
            if state == "blank" {
                assert!(driver
                    .get_timeline(&info.asid.0, 10)
                    .await
                    .unwrap()
                    .is_empty());
                driver.prepare_selected_session(&info.asid.0).await.unwrap();
                assert!(driver.host.get(&info.asid.0).is_some());
            }
        }
        driver.host.shutdown_all().await;
    }

    /// Opt-in live model contract. Its entire workspace and session root
    /// are test-owned; tools are disabled and no approval is answered.
    #[tokio::test]
    #[ignore]
    async fn real_pi_idle_exit_and_same_id_context_resume() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("muqun-pi-idle-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("sessions")).unwrap();
        let extension = root.join("muqun-extension.js");
        std::fs::write(&extension, mapper::EXTENSION_SOURCE).unwrap();
        let binary = root.join("pi-no-tools");
        std::fs::write(&binary, "#!/bin/sh\nexec pi --no-tools \"$@\"\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let driver = PiDriver::new(
            PiCommandLine { binary, extension },
            SessionStore::new(root.join("sessions")),
            true,
            Duration::from_secs(2),
            vec![(
                "PI_CODING_AGENT_SESSION_DIR".into(),
                root.join("sessions").to_string_lossy().into_owned(),
            )],
        );
        let mut events = driver.subscribe();
        let info = driver
            .create_session(root.to_str(), None, None)
            .await
            .unwrap();
        let asid = info.asid.0;
        let blank = driver.host.get(&asid).unwrap();
        let blank_pid = blank.pid().unwrap();
        reap_once(&driver.host, &driver.live, Duration::ZERO).await;
        assert!(
            !blank.is_alive(),
            "blank process kept alive without a native file"
        );
        assert!(driver.get_timeline(&asid, 100).await.unwrap().is_empty());
        assert!(driver
            .list_sessions(&SessionQuery::default())
            .await
            .unwrap()
            .iter()
            .any(|s| s.asid.0 == asid));
        assert!(driver.host.get(&asid).is_none());
        driver.prepare_selected_session(&asid).await.unwrap();
        assert_eq!(driver.get_session(&asid).await.unwrap().model, info.model);
        let first = driver.host.get(&asid).unwrap();
        assert_ne!(first.pid().unwrap(), blank_pid);
        let first_pid = first.pid().unwrap();
        let first_rss = std::fs::read_to_string(format!("/proc/{first_pid}/status"))
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find(|line| line.starts_with("VmRSS:"))
                    .map(str::to_owned)
            });
        let marker = format!("PI-IDLE-{}", uuid::Uuid::new_v4());
        driver.send_prompt(&asid, &format!("Remember this exact marker for this conversation: {marker}. Reply with only SAVED. Do not use any tools."), &[], None).await.unwrap();
        real_turn_settled(&driver, &mut events).await;
        let before = driver.get_timeline(&asid, 100).await.unwrap();
        assert!(!before.is_empty());
        // Repeated observational polling crosses the configured QA timeout.
        for _ in 0..12 {
            driver.get_session(&asid).await.unwrap();
            driver.get_timeline(&asid, 100).await.unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        reap_once(&driver.host, &driver.live, Duration::from_secs(2)).await;
        assert!(driver.host.get(&asid).is_none(), "idle Pi was not retired");
        assert!(!first.is_alive());
        assert_eq!(
            unsafe { libc::kill(first_pid as i32, 0) },
            -1,
            "original PID still exists"
        );
        assert!(
            driver.live.with(&asid, |_| ()).is_none(),
            "retired live projection retained"
        );
        driver.get_session(&asid).await.unwrap();
        driver.get_timeline(&asid, 100).await.unwrap();
        assert!(
            !driver.host.any_alive(),
            "background observation respawned Pi"
        );
        driver.prepare_selected_session(&asid).await.unwrap();
        let second = driver.host.get(&asid).unwrap();
        let second_pid = second.pid().unwrap();
        assert_ne!(first_pid, second_pid);
        assert_eq!(driver.get_timeline(&asid, 100).await.unwrap(), before);
        driver.send_prompt(&asid, "What exact marker did I ask you to remember? Reply only with the marker. Do not use any tools.", &[], None).await.unwrap();
        real_turn_settled(&driver, &mut events).await;
        let file = driver.store.find(&asid).await.unwrap();
        let entries = driver.store.read_entries(&file).await.unwrap();
        let last_assistant = entries
            .iter()
            .rev()
            .find(|e| e.pointer("/message/role").and_then(Value::as_str) == Some("assistant"))
            .unwrap();
        assert!(
            last_assistant.to_string().contains(&marker),
            "resumed context lost marker"
        );
        eprintln!("Pi idle QA: asid={asid} blank_pid={blank_pid} blank_retired_and_recreated=true first_pid={first_pid} first_rss={first_rss:?} absent_after_idle=true second_pid={second_pid} history_before={} marker_recalled=true root={}", before.len(), root.display());
        driver.shutdown_and_wait().await;
    }

    async fn real_turn_settled(
        driver: &PiDriver,
        events: &mut broadcast::Receiver<(AgentSessionId, u64, Arc<PiRecord>)>,
    ) {
        tokio::time::timeout(Duration::from_secs(180), async {
            let mut settled = false;
            loop {
                match tokio::time::timeout(Duration::from_millis(250), events.recv()).await {
                    Ok(Ok((id, generation, record))) => {
                        assert_ne!(
                            record.get("type").and_then(Value::as_str),
                            Some("extension_ui_request"),
                            "live Pi requested approval; no answer sent"
                        );
                        settled |=
                            record.get("type").and_then(Value::as_str) == Some("agent_settled");
                        driver.fold(&id, generation, &record).await;
                    }
                    Err(_) if settled => break,
                    Err(_) => {}
                    other => panic!("Pi event stream failed: {other:?}"),
                }
            }
        })
        .await
        .expect("live Pi turn settles");
    }

    /// A failing `pi --version` does not count against the agent while a
    /// child is running; with none, it does.
    #[tokio::test]
    async fn probe_trusts_a_live_child_over_the_binary() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        for (alive, healthy) in [(true, true), (false, false)] {
            if !alive {
                driver.delete_session(&info.asid.0).await.unwrap();
            }
            assert_eq!(driver.probe().await.unwrap(), healthy, "alive={alive}");
        }
    }

    #[tokio::test]
    async fn prompt_failure_is_never_replayed() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        std::fs::write(dir.join("reject-prompt"), "").unwrap();
        let error = driver
            .send_prompt(&info.asid.0, "submit once", &[], None)
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::RequestFailed(_)));
        let commands = std::fs::read_to_string(dir.join("commands.jsonl")).unwrap();
        let prompts = commands
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|c| c["type"] == "prompt")
            .collect::<Vec<_>>();
        assert_eq!(prompts.len(), 1);
        assert!(prompts[0].get("streamingBehavior").is_none());
        driver.host.shutdown_all().await;
    }

    #[tokio::test]
    async fn invalid_native_uploads_fail_before_starting_or_sending_to_a_child() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        assert_eq!(driver.attachment_mode(), AttachmentMode::NativeUploads);
        let error = driver
            .send_prompt(
                "not-yet-started",
                "do not send a partial prompt",
                &["not-an-upload.png".into()],
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, AgentError::InvalidRequest(_)));
        assert!(!driver.host.any_alive());
        assert!(!dir.join("commands.jsonl").exists());
    }

    #[tokio::test]
    async fn old_generation_cannot_change_status_or_create_approvals() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        let generation = driver.live.with(&info.asid.0, |s| s.generation).unwrap();
        for record in [
            json!({"type":"agent_start"}),
            json!({"type":"extension_ui_request", "id":"old", "method":"confirm", "title":"muqun.permission", "message":"{\"tool\":\"bash\"}"}),
        ] {
            assert!(driver
                .fold(&info.asid, generation + 1, &record)
                .await
                .is_empty());
        }
        assert_eq!(driver.status_of(&info.asid.0), AgentSessionStatus::Idle);
        assert!(driver
            .get_pending_permissions(&info.asid.0)
            .await
            .unwrap()
            .is_empty());
        driver.host.shutdown_all().await;
    }

    #[tokio::test]
    async fn fresh_sessions_keep_the_effective_model_and_thinking_level() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let requested = ModelRef {
            provider_id: "example".into(),
            model_id: "chosen".into(),
            variant: Some("xhigh".into()),
        };
        let info = driver
            .create_session(dir.to_str(), Some(&requested), None)
            .await
            .unwrap();
        let expected = ModelRef {
            variant: Some("high".into()),
            ..requested.clone()
        };
        assert_eq!(info.model, Some(expected.clone()));
        assert_eq!(
            driver.get_session(&info.asid.0).await.unwrap().model,
            Some(expected)
        );
        let switched = ModelRef {
            variant: Some("low".into()),
            ..requested
        };
        let mut updates = driver.subscribe();
        driver.switch_model(&info.asid.0, &switched).await.unwrap();
        let (_, _, published) = tokio::time::timeout(Duration::from_secs(2), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(published["type"], "muqun_model");
        assert_eq!(published["model"]["variant"], "low");
        assert_eq!(
            driver.get_session(&info.asid.0).await.unwrap().model,
            Some(switched)
        );
        std::fs::write(dir.join("reject-thinking"), "").unwrap();
        let partial = ModelRef {
            provider_id: "example".into(),
            model_id: "changed-before-error".into(),
            variant: Some("high".into()),
        };
        assert!(driver.switch_model(&info.asid.0, &partial).await.is_err());
        assert_eq!(
            driver.get_session(&info.asid.0).await.unwrap().model,
            Some(ModelRef {
                variant: Some("low".into()),
                ..partial
            })
        );
        driver.host.shutdown_all().await;
    }

    #[tokio::test]
    async fn busy_slash_commands_use_prompt_once_and_unlisted_commands_are_refused() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        driver
            .live
            .with(&info.asid.0, |s| s.status = AgentSessionStatus::Busy);
        driver
            .run_command(&info.asid.0, "example", "argument", Some("steer"))
            .await
            .unwrap();
        assert!(driver
            .run_command(&info.asid.0, "settings", "", None)
            .await
            .is_err());
        let commands = std::fs::read_to_string(dir.join("commands.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let submitted = commands
            .iter()
            .filter(|c| c["message"] == "/example argument")
            .collect::<Vec<_>>();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0]["type"], "prompt");
        assert_eq!(submitted[0]["streamingBehavior"], "steer");
        assert!(!commands.iter().any(|c| c["message"] == "/settings"));
        driver.host.shutdown_all().await;
    }

    #[tokio::test]
    async fn whole_session_fork_uses_a_separate_child_and_preserves_the_source() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let source = uuid::Uuid::new_v4().to_string();
        let path = dir
            .join("sessions")
            .join(format!("2026-10-14T06-51-18-225Z_{source}.jsonl"));
        let body = format!(
            "{}\n{}\n",
            json!({"type":"session", "id":source, "cwd":dir.to_str().unwrap(), "timestamp":"2026-10-14T06:51:18.225Z"}),
            json!({"type":"message", "id":"entry", "parentId":null, "timestamp":"2026-10-14T06:51:18.225Z", "message":{"role":"user", "content":"kept", "timestamp":1}})
        );
        std::fs::write(&path, &body).unwrap();
        let source_child = driver.child_for(&source).await.unwrap();
        source_child
            .request(json!({"type":"get_state"}))
            .await
            .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        let cloned = driver.fork_session(&source, None).await.unwrap();
        assert_ne!(cloned.asid.0, source);
        assert_eq!(cloned.fork.unwrap().session_id, source);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(std::ptr::eq(
            &*source_child,
            &*driver.host.get(&source).unwrap()
        ));
        assert!(source_child.is_alive());
        assert!(
            driver.host.get(&cloned.asid.0).is_none(),
            "clone probe must not become an ungated runtime"
        );
        assert!(driver.fork_session(&source, Some("entry")).await.is_err());
        driver.host.shutdown_all().await;
    }

    #[tokio::test]
    async fn backlog_failure_closes_children_and_resolves_pending_approvals() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        let generation = driver.live.with(&info.asid.0, |s| s.generation).unwrap();
        driver
            .fold(&info.asid, generation, &json!({"type":"agent_start"}))
            .await;
        let payload = json!({"tool":"bash", "toolCallId":"isolated", "input":{"command":"probe"}})
            .to_string();
        driver.fold(&info.asid, generation, &json!({"type":"extension_ui_request", "id":"approval", "method":"confirm", "title":"muqun.permission", "message":payload})).await;
        assert!(driver
            .reply_permission(
                &info.asid.0,
                "approval",
                PermissionDecision::AllowAlways,
                None
            )
            .await
            .is_err());
        let changes = driver.abort_backlog().await;
        assert!(changes
            .iter()
            .flat_map(|(_, c)| c)
            .any(|c| matches!(c, LiveChange::PermissionResolved(id) if id == "approval")));
        assert!(driver
            .get_pending_permissions(&info.asid.0)
            .await
            .unwrap()
            .is_empty());
        assert!(!driver.host.any_alive());
        assert!(!driver.probe().await.unwrap());
        assert!(driver
            .send_prompt(&info.asid.0, "do not send", &[], None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn shutdown_and_backlog_failure_close_admission_before_waiting_for_startup() {
        for backlog in [false, true] {
            let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
                return;
            };
            let driver = Arc::new(driver);
            let held = driver.spawn_lock.lock().await;
            let starting = driver.clone();
            let directory = dir.clone();
            let create = tokio::spawn(async move {
                starting
                    .create_session(directory.to_str(), None, None)
                    .await
            });
            tokio::task::yield_now().await;
            let stopping = driver.clone();
            let stop = tokio::spawn(async move {
                if backlog {
                    stopping.abort_backlog().await;
                } else {
                    stopping.shutdown_and_wait().await;
                }
            });
            tokio::time::timeout(Duration::from_secs(2), driver.stop.cancelled())
                .await
                .unwrap();
            drop(held);
            assert!(create.await.unwrap().is_err());
            stop.await.unwrap();
            assert!(!driver.is_connected());
            assert!(!driver.host.any_alive());
            assert!(
                !dir.join("commands.jsonl").exists(),
                "no child may start after admission closed"
            );
            assert!(driver
                .create_session(dir.to_str(), None, None)
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn new_session_discovers_its_file_after_the_first_message_and_rekeys_the_echo() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let info = driver
            .create_session(dir.to_str(), None, None)
            .await
            .unwrap();
        let generation = driver
            .live
            .with(&info.asid.0, |s| {
                s.file = None;
                s.generation
            })
            .unwrap();
        let message = json!({"role":"user", "content":"one delivery", "timestamp":100});
        driver
            .fold(
                &info.asid,
                generation,
                &json!({"type":"message_end", "message":message}),
            )
            .await;
        let path = dir
            .join("sessions")
            .join(format!("2026-10-14T06-51-18-225Z_{}.jsonl", info.asid.0));
        let rows = [
            json!({"type":"session","id":info.asid.0,"cwd":dir.to_str().unwrap(),"timestamp":"2026-10-14T06:51:18.225Z"}),
            json!({"type":"message","id":"native-entry","parentId":null,"timestamp":"2026-10-14T06:51:19.000Z","message":message}),
        ];
        std::fs::write(
            &path,
            rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
        )
        .unwrap();
        for entry in driver
            .live
            .read_tail(&info.asid.0, generation)
            .await
            .unwrap()
        {
            driver
                .fold(
                    &info.asid,
                    generation,
                    &json!({"type":"entry_appended", "entry":entry}),
                )
                .await;
        }
        assert_eq!(
            driver.live.with(&info.asid.0, |s| s.file.clone()).flatten(),
            Some(path)
        );
        let rows = driver.live.with(&info.asid.0, |s| s.rows.clone()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "native-entry");
        driver.shutdown_and_wait().await;
    }

    #[tokio::test]
    async fn file_history_pages_commit_only_after_acceptance_and_survive_refresh() {
        let Some((driver, dir)) = fake_driver(Duration::from_secs(900)) else {
            return;
        };
        let mut lines = vec![
            json!({"type":"session", "id":"history", "cwd":dir.to_str().unwrap(), "timestamp":"2026-10-14T06:51:18.225Z"}),
        ];
        for n in 0..250 {
            lines.push(json!({"type":"message", "id":format!("row-{n}"), "parentId":if n == 0 { Value::Null } else { json!(format!("row-{}", n-1)) }, "timestamp":"2026-10-14T06:51:18.225Z", "message":{"role":"user", "timestamp":n, "content":format!("message {n}")}}));
        }
        let path = dir.join("sessions/2026-10-14T06-51-18-225Z_history.jsonl");
        let body = lines.iter().map(|v| format!("{v}\n")).collect::<String>();
        std::fs::write(path, body).unwrap();
        let discarded = driver.prepare_timeline("history", 100).await.unwrap();
        assert_eq!(discarded.items.first().unwrap().id, "row-150");
        drop(discarded);
        assert!(matches!(
            driver.prepare_earlier_timeline("history", 80).await,
            Err(AgentError::HistoryCursorLost)
        ));
        let mut initial = driver.prepare_timeline("history", 100).await.unwrap();
        initial.commit.take().unwrap()().unwrap();
        let unaccepted = driver
            .prepare_earlier_timeline("history", 80)
            .await
            .unwrap();
        assert_eq!(unaccepted.items.first().unwrap().id, "row-70");
        drop(unaccepted);
        let mut first = driver
            .prepare_earlier_timeline("history", 80)
            .await
            .unwrap();
        assert_eq!(first.items.first().unwrap().id, "row-70");
        first.commit.take().unwrap()().unwrap();
        let mut refresh = driver.prepare_timeline("history", 100).await.unwrap();
        refresh.commit.take().unwrap()().unwrap();
        let mut second = driver
            .prepare_earlier_timeline("history", 80)
            .await
            .unwrap();
        assert_eq!(second.items.first().unwrap().id, "row-0");
        assert_eq!(second.items.len(), 70);
        let mut racing = driver
            .prepare_earlier_timeline("history", 80)
            .await
            .unwrap();
        second.commit.take().unwrap()().unwrap();
        assert!(matches!(
            racing.commit.take().unwrap()(),
            Err(AgentError::HistoryCursorLost)
        ));
        assert!(driver
            .load_earlier_timeline("history", 80)
            .await
            .unwrap()
            .is_empty());
    }
}
