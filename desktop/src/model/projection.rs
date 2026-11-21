//! The core side of the boundary: runs on the tokio runtime, turns
//! [`Command`]s into hub calls and hub events into inbox [`Update`]s.
//!
//! Nothing here touches Blinc except through the inbox's wake-up.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use muqun_gateway::agents::ports::AgentError;
use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId, SessionQuery};
use tokio::sync::{broadcast, watch};

use super::commands::{Command, CommandReceiver};
use super::compose::{self, ComposeCommand, ComposerUpdate, Deliver, Target};
use super::inbox::{Inbox, SearchOutcome, UiError, Update};
use super::types::{DiffStat, SESSION_LIST_LIMIT};
use crate::config::{ConfigEdit, DesktopConfig};
use crate::hub::{DynamicHub, HubError, SessionHub, TaskHubEvent, OPENCODE};

/// How long the typed prefix must stand still before folders are listed.
const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);
/// The quiet after the last keystroke before the sidebar searches message content.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(200);
/// How long the configuration must stand still before it is written.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// One agent's session list, at most.
/// Local agents answer in milliseconds; a hung adapter must not hold the
/// sidebar for half a minute.
const LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// How long after startup newly attached agents are looked for: the gateway
/// probes cold agents for up to `DISCOVERY_TIMEOUT` (60 s).
const WATCH_AGENTS_FOR: Duration = Duration::from_secs(90);
const ATTACH_POLL: Duration = Duration::from_secs(2);
const DISCOVERY_POLL: Duration = Duration::from_secs(5);
/// How often the open session is marked read again while it stays open.
const VIEW_HEARTBEAT: Duration = Duration::from_secs(30);

/// The configuration file and its current contents, shared by the core side
/// and, for reading and the last write at exit, the main thread.
pub struct ConfigStore {
    path: PathBuf,
    state: Mutex<(DesktopConfig, bool)>,
}

impl ConfigStore {
    pub fn new(path: PathBuf, config: DesktopConfig) -> Arc<Self> {
        Arc::new(Self {
            path,
            state: Mutex::new((config, false)),
        })
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn get(&self) -> DesktopConfig {
        self.state.lock().unwrap().0.clone()
    }

    /// Background connection maintenance does not need to clone drafts or UI state.
    pub(super) fn computer_profiles(&self) -> HashMap<String, crate::config::ComputerProfile> {
        self.state.lock().unwrap().0.computers.clone()
    }

    /// Change the configuration in memory and mark it unwritten.
    pub fn change(&self, change: impl FnOnce(&mut DesktopConfig)) {
        let mut state = self.state.lock().unwrap();
        change(&mut state.0);
        state.1 = true;
    }

    /// Write the file if anything changed since the last write. Blocks on
    /// the disk: call it off the async threads (or at exit).
    pub fn flush(&self) {
        let snapshot = {
            let mut state = self.state.lock().unwrap();
            if !std::mem::take(&mut state.1) {
                return;
            }
            state.0.clone()
        };
        if let Err(err) = snapshot.save(&self.path) {
            // Keep it dirty so the next change or exit tries again.
            self.state.lock().unwrap().1 = true;
            tracing::warn!("{err:#}");
        }
    }
}

/// How long all the existence checks of one project list may take together;
/// a folder on a stalled mount is left unmarked rather than held against it.
const EXISTS_BUDGET: Duration = Duration::from_secs(2);

/// Set `missing` on projects whose folder is gone, for agents (and old
/// entries) whose own listing does not say.
async fn mark_missing(projects: &mut [muqun_gateway::agents::AgentProject]) {
    let check = async {
        for project in projects.iter_mut() {
            let path = std::path::Path::new(&project.canonical);
            if !project.missing
                && path.is_absolute()
                && matches!(tokio::fs::try_exists(path).await, Ok(false))
            {
                project.missing = true;
            }
        }
    };
    let _ = tokio::time::timeout(EXISTS_BUDGET, check).await;
}

/// Refreshes asked for within this of the last are folded into it.
const REFRESH_DEDUPE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether the UI can show anything of `event`, with `active` the session
/// that is open. Every agent's sessions stream through one channel, and an
/// agent working in a session nobody is looking at sends its timeline many
/// times a second; queueing those would wake the window for nothing. What a
/// listed row shows (title, status, order, a pending approval) always goes
/// through; the rest only for the open session -- its snapshot, read after it
/// became the active one, covers anything dropped before.
fn wanted(
    event: &AgentDomainEvent,
    active: Option<&AgentSessionId>,
    watched: &[AgentSessionId],
) -> bool {
    match event {
        AgentDomainEvent::SessionUpdated { .. }
        | AgentDomainEvent::StatusChanged { .. }
        | AgentDomainEvent::PermissionPending { .. }
        | AgentDomainEvent::PermissionResolved { .. }
        // The Inbox counts every session's open forms.
        | AgentDomainEvent::FormPending { .. }
        | AgentDomainEvent::FormResolved { .. }
        | AgentDomainEvent::Resync { .. } => true,
        other => {
            active.is_some_and(|active| active == other.asid()) || watched.contains(other.asid())
        }
    }
}

/// One kind of re-read that must not pile up: while one runs, any number of
/// further requests fold into a single re-run after it finishes.
#[derive(Default)]
struct Coalesce {
    /// (running, asked again while running)
    state: Mutex<(bool, bool)>,
}

impl Coalesce {
    /// True when the caller should start the work now; false when a run is
    /// in flight and will go once more after it.
    fn begin(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.0 {
            state.1 = true;
            false
        } else {
            state.0 = true;
            true
        }
    }

    /// End a run. True when it was asked for again meanwhile and must run
    /// once more.
    fn again(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        if std::mem::take(&mut state.1) {
            true
        } else {
            state.0 = false;
            false
        }
    }
}

/// Mutable read/view bookkeeping belongs to one connection generation.
/// In-flight work may retain an old state, but cannot write the next connection's state.
#[derive(Default)]
struct ConnectionState {
    /// The session the user is looking at, for resyncs.
    active: Mutex<Option<AgentSessionId>>,
    /// Sessions open in split tiles beside the active one: their events
    /// stream too (`Command::WatchSessions`).
    watched: Mutex<Vec<AgentSessionId>>,
    /// Re-reads of the active session's snapshot, after a resync.
    reread: Coalesce,
    /// Re-reads of every agent's list, after a resync.
    relist: Coalesce,
    /// Re-reads of the task list, after a task event.
    retask: Coalesce,
    /// Session folders already resolved to their projects.
    resolved: Mutex<std::collections::HashSet<String>>,
    /// The newest sequence handed to the UI per session, for catching up
    /// after a resync.
    last_seq: Mutex<HashMap<String, u64>>,
    /// Which view-heartbeat task is current; older ones end at their next tick.
    beat: AtomicU64,
    /// When the last full refresh started: boot asks from main and from the
    /// window, and a second ask this soon is answered by the first.
    refreshed_at: Mutex<Option<std::time::Instant>>,
    tasks_reading: std::sync::atomic::AtomicBool,
    /// Gateway pictures being fetched into the media cache.
    fetching: Arc<Mutex<std::collections::HashSet<String>>>,
}

struct Core {
    connection: Mutex<crate::hub::ConnectionContext>,
    local_hub: Option<Arc<dyn SessionHub>>,
    local_state_dir: Option<PathBuf>,
    fleet: Arc<super::fleet_attention::Fleet>,
    connection_generation: AtomicU64,
    connected: std::sync::atomic::AtomicBool,
    subscriptions: Mutex<Vec<tokio::task::AbortHandle>>,
    hub: Arc<DynamicHub>,
    inbox: Arc<Inbox>,
    config: Arc<ConfigStore>,
    state: Mutex<(u64, Arc<ConnectionState>)>,
    completions: watch::Sender<Option<(crate::hub::ConnectionContext, String)>>,
    /// Bumped on every edit; the saver waits for it to go quiet.
    edits: watch::Sender<u64>,
}

/// Start the core side. Returns at once; the tasks live as long as the
/// runtime does.
pub fn spawn(
    hub: Arc<DynamicHub>,
    inbox: Arc<Inbox>,
    config: Arc<ConfigStore>,
    commands: CommandReceiver,
    initial_connected: bool,
    local_state_dir: PathBuf,
) {
    let (completions, completion_rx) = watch::channel(None);
    let (edits, edits_rx) = watch::channel(0);
    let computer = match config.get().current_computer.as_str() {
        "" | "self" | "all" => "local".to_string(),
        other => other.to_string(),
    };
    let connection = hub.connection(computer, 0);
    let fleet = Arc::new(super::fleet_attention::Fleet::default());
    fleet.insert(
        if initial_connected {
            connection.computer.clone()
        } else {
            "local".into()
        },
        connection.hub.clone(),
        inbox.clone(),
        &config,
    );
    tokio::spawn(fleet.clone().maintain(config.clone(), inbox.clone()));
    let core = Arc::new(Core {
        fleet,
        local_state_dir: Some(local_state_dir),
        local_hub: (connection.computer == "local" || !initial_connected)
            .then(|| connection.hub.clone()),
        connection: Mutex::new(connection),
        connection_generation: AtomicU64::new(0),
        connected: std::sync::atomic::AtomicBool::new(initial_connected),
        subscriptions: Mutex::new(Vec::new()),
        hub,
        inbox,
        config,
        state: Mutex::new((0, Arc::new(ConnectionState::default()))),
        completions,
        edits,
    });
    if initial_connected {
        core.start_subscriptions();
    }
    tokio::spawn(core.clone().complete_directories(completion_rx));
    tokio::spawn(core.clone().save_config(edits_rx));
    if let Ok(list) = std::env::var("MUQUN_QA_SWITCH") {
        tokio::spawn(core.clone().qa_switch(list));
    }
    tokio::spawn(core.commands(commands));
}

impl Core {
    fn state(&self) -> Arc<ConnectionState> {
        let state = self.state.lock().unwrap();
        match crate::hub::connection_origin() {
            Some((_, generation)) if generation != state.0 => Arc::new(ConnectionState::default()),
            _ => state.1.clone(),
        }
    }

    fn connection_is_current(&self) -> bool {
        crate::hub::connection_origin()
            .is_none_or(|(_, generation)| generation == self.state.lock().unwrap().0)
    }

    fn start_subscriptions(self: &Arc<Self>) {
        let core = self.clone();
        let context = self.connection.lock().unwrap().clone();
        let task = tokio::spawn(context.scope(async move {
            core.inbox.push(Update::HubMode(core.hub.mode()));
            let task_events = async {
                if let Some(events) = core.hub.task_events() {
                    core.clone().task_events(events).await;
                }
            };
            let link_events = async {
                if let Some(mut link) = core.hub.link() {
                    core.inbox
                        .push(Update::HubLink(link.borrow_and_update().clone()));
                    while link.changed().await.is_ok() {
                        core.inbox
                            .push(Update::HubLink(link.borrow_and_update().clone()));
                    }
                }
            };
            tokio::join!(
                core.clone().events(),
                core.clone().watch_agents(),
                task_events,
                link_events
            );
        }));
        self.subscriptions.lock().unwrap().push(task.abort_handle());
    }

    async fn commands(self: Arc<Self>, mut commands: CommandReceiver) {
        // The content search in flight: a newer one cancels it.
        let mut search: Option<tokio::task::AbortHandle> = None;
        while let Some(command) = commands.recv().await {
            let core = self.clone();
            match command {
                Command::Search { generation, query } => {
                    if let Some(stale) = search.take() {
                        stale.abort();
                    }
                    let connection = self.connection.lock().unwrap().clone();
                    let task = tokio::spawn(
                        connection.scope(async move { core.search(generation, query).await }),
                    );
                    search = Some(task.abort_handle());
                }
                Command::CompleteDirectory { prefix } => {
                    let connection = self.connection.lock().unwrap().clone();
                    let _ = self.completions.send(Some((connection, prefix)));
                }
                // In this loop, not a task: edits must apply in the order sent.
                Command::Edit(edit) => self.edit(edit),
                Command::SwitchComputer(target) => {
                    if let Some(stale) = search.take() {
                        stale.abort();
                    }
                    self.clone().switch_computer(target).await;
                }
                command => {
                    let connection = self.connection.lock().unwrap().clone();
                    tokio::spawn(connection.scope(async move { core.run(command).await }));
                }
            }
        }
        tracing::debug!("the UI closed its command channel");
    }

    /// One content search; a first page waits out the typing first.
    async fn search(&self, generation: u64, query: crate::hub::SearchQuery) {
        if !self.connected.load(Ordering::Acquire) {
            self.inbox.push(Update::Search {
                generation,
                outcome: SearchOutcome::Failed,
            });
            return;
        }
        if query.cursor.is_none() {
            tokio::time::sleep(SEARCH_DEBOUNCE).await;
        }
        let outcome = match self.hub.search(&query).await {
            Ok(page) => SearchOutcome::Page(page),
            Err(HubError::Agent(AgentError::Unsupported(_))) => SearchOutcome::Unsupported,
            Err(err) => {
                tracing::debug!(%err, "content search failed");
                SearchOutcome::Failed
            }
        };
        self.inbox.push(Update::Search {
            generation,
            outcome,
        });
    }

    async fn run(self: Arc<Self>, command: Command) {
        if !self.connected.load(Ordering::Acquire)
            && !matches!(
                command,
                Command::SwitchComputer(_)
                    | Command::PairComputerRequest { .. }
                    | Command::PairComputerClaim { .. }
                    | Command::PairComputerToken { .. }
                    | Command::Voice(_)
                    | Command::ThemeRefresh
                    | Command::ThemeInstall(_)
                    | Command::ThemeRemove(_)
                    | Command::ThemeApply(..)
                    | Command::ThemeImportFile(_)
                    | Command::ThemeImportUrl(_)
                    | Command::UnpairComputer { .. }
                    | Command::RevokeLocalPairing(_)
                    | Command::ReadLocalInvite(_)
                    | Command::CheckDesktopUpdate
                    | Command::RefreshLocalPairings
                    | Command::ScanStorage
                    | Command::ClearStorage
                    | Command::PluginPackage(..)
                    | Command::ProbeRuntimes
                    | Command::InstallManagedRuntime(_)
                    | Command::MaoPrepareReview { .. }
            )
        {
            if let Command::OpenSession(asid) | Command::OpenTile(asid) = &command {
                self.inbox.push(Update::SessionOpenFailed {
                    asid: asid.0.clone(),
                    message: "Computer connection failed. Select the computer again to reconnect."
                        .into(),
                });
            }
            if let Command::Compose(ComposeCommand::Submit { token, .. }) = command {
                self.inbox
                    .push(Update::Composer(Box::new(ComposerUpdate::Submitted {
                        token,
                        ok: false,
                    })));
            }
            return;
        }
        match command {
            Command::Refresh => self.refresh().await,
            Command::SwitchComputer(target) => self.switch_computer(target).await,
            Command::PairComputerRequest {
                attempt,
                server_id,
                url,
            } => {
                let result = async {
                    let offer =
                        crate::platform::pairing::request_app_pairing(&url, "Muqun Desktop")
                            .await?;
                    crate::platform::pairing::validate_identity(&server_id, &offer.server_id)?;
                    anyhow::Ok(offer)
                }
                .await
                .map_err(|e| e.to_string());
                self.inbox.push(Update::ComputerPairingRequested {
                    attempt,
                    url,
                    result,
                });
            }
            Command::PairComputerClaim {
                attempt,
                server_id,
                url,
                request_id,
                code,
                transport_required,
            } => {
                let result = async {
                    let claimed = crate::platform::pairing::claim_app_pairing(
                        &url,
                        &request_id,
                        &code,
                        transport_required,
                    )
                    .await?;
                    crate::platform::pairing::validate_identity(&server_id, &claimed.server_id)?;
                    anyhow::Ok(claimed)
                }
                .await
                .map_err(|e| e.to_string());
                self.inbox.push(Update::ComputerPairingOutcome {
                    attempt,
                    url,
                    result,
                });
            }
            Command::PairComputerToken {
                attempt,
                server_id,
                url,
                token,
            } => {
                let result = async {
                    let verified = crate::platform::pairing::verify_token(&url, &token).await?;
                    crate::platform::pairing::validate_identity(&server_id, &verified.server_id)?;
                    anyhow::Ok(verified)
                }
                .await
                .map_err(|e| e.to_string());
                self.inbox.push(Update::ComputerPairingOutcome {
                    attempt,
                    url,
                    result,
                });
            }
            Command::OpenSession(asid) => {
                *self.state().active.lock().unwrap() = Some(asid.clone());
                self.open(&asid).await;
            }
            Command::ReadDetail(asid) => {
                let read = match self.hub.peek(&asid).await {
                    Ok(Some(snapshot)) => Ok(snapshot),
                    _ => self.hub.snapshot(&asid).await,
                };
                self.inbox.push(Update::Detail {
                    asid: asid.0,
                    result: read.map(Box::new).map_err(|err| err.to_string()),
                });
            }
            Command::LoadAccountLimits { agent } => {
                let limits = match self.hub.account_limits(&agent).await {
                    Ok(limits) => Some(limits),
                    Err(err) => {
                        tracing::debug!(%agent, %err, "account limits unavailable");
                        None
                    }
                };
                self.inbox.push(Update::AccountLimits { agent, limits });
            }
            Command::OpenTile(asid) => {
                {
                    let state = self.state();
                    let mut watched = state.watched.lock().unwrap();
                    if !watched.contains(&asid) {
                        watched.push(asid.clone());
                    }
                }
                self.open(&asid).await;
            }
            Command::FocusSession(asid) => {
                *self.state().active.lock().unwrap() = Some(asid.clone());
                self.watch_viewed(&asid);
                crate::hub::spawn_connected(self.clone().diff(asid.clone()));
                crate::hub::spawn_connected(self.clone().descendants(asid.clone()));
            }
            Command::WatchSessions(asids) => {
                *self.state().watched.lock().unwrap() = asids;
            }
            Command::CreateSession { agent, cwd } => match self.hub.create(&agent, &cwd).await {
                Ok(info) => {
                    self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                    self.inbox
                        .push(Update::SessionCreated(Box::new(info), None));
                    self.config.change(|config| {
                        config.general.last_agent = Some(agent.clone());
                        if crate::hub::connection_origin()
                            .is_none_or(|(computer, _)| computer == "local")
                        {
                            config.general.last_cwd.insert(agent, cwd);
                        }
                    });
                    self.edited();
                }
                Err(err) => self.fail(err),
            },
            Command::SendPrompt { asid, text } => {
                if let Err(err) = self.hub.prompt(&asid, &text).await {
                    self.fail(err);
                }
            }
            // Taken by the command loop, which cancels a stale one.
            Command::Search { .. } => {}
            Command::Interrupt(asid) => {
                if let Err(err) = self.hub.interrupt(&asid).await {
                    self.fail(err);
                }
            }
            Command::ReplyPermission { asid, id, decision } => {
                if let Err(err) = self.hub.reply_permission(&asid, &id, decision).await {
                    self.fail(err);
                }
            }
            Command::LoadEarlier(asid) => match self.hub.earlier(&asid).await {
                Ok(mut items) => {
                    self.ready_rows(&mut items);
                    let exhausted = items.is_empty();
                    tracing::debug!(asid = %asid.0, rows = items.len(), "older history page");
                    self.inbox.push(Update::Earlier {
                        asid: asid.0,
                        items,
                        exhausted,
                    });
                }
                Err(HubError::Agent(AgentError::HistoryCursorLost)) => {
                    // The cursor went with the snapshot it belonged to:
                    // read the session again, then page on from there.
                    // Re-read, never prepare: preparing a terminal-held ACP
                    // session starts another transcript follower (see
                    // `resync`); the hub falls back to the session service
                    // only when the mirror no longer holds the session.
                    tracing::info!(asid = %asid.0, "history cursor lost; reading the session again");
                    match self.hub.snapshot(&asid).await {
                        Ok(snapshot) => self.push_snapshot(snapshot),
                        Err(err) => tracing::debug!(%err, "re-read after a lost cursor failed"),
                    }
                    // Answer the request all the same: the list is waiting on
                    // it, and asks again after a pause.
                    self.inbox.push(Update::Earlier {
                        asid: asid.0,
                        items: Vec::new(),
                        exhausted: false,
                    });
                }
                Err(HubError::Agent(AgentError::Unsupported(_))) => {
                    self.inbox.push(Update::Earlier {
                        asid: asid.0,
                        items: Vec::new(),
                        exhausted: true,
                    });
                }
                Err(err) => {
                    // Leave the session pageable; the next approach to the
                    // top asks again.
                    tracing::warn!(asid = %asid.0, %err, "older history unavailable");
                    self.inbox.push(Update::Earlier {
                        asid: asid.0,
                        items: Vec::new(),
                        exhausted: false,
                    });
                }
            },
            Command::OpenDiff { asid, file } => match file {
                Some(path) => self
                    .inbox
                    .push(Update::Ui(super::inbox::UiAction::OpenDiffFile {
                        asid: asid.0,
                        path,
                    })),
                None => self
                    .inbox
                    .push(Update::Ui(super::inbox::UiAction::ToggleDiffPanel)),
            },
            Command::RefreshDiff(asid) => {
                crate::hub::spawn_connected(self.clone().diff(asid));
            }
            Command::StageRevert { asid, message_id } => {
                if let Err(err) = self.hub.stage_revert(&asid, &message_id, true).await {
                    self.fail(err);
                }
            }
            Command::CommitRevert { asid } => {
                if let Err(err) = self.hub.commit_revert(&asid).await {
                    self.fail(err);
                }
            }
            Command::ClearRevert { asid } => {
                if let Err(err) = self.hub.clear_revert(&asid).await {
                    self.fail(err);
                }
            }
            Command::CompactSession { asid } => {
                if let Err(err) = self.hub.compact(&asid, None).await {
                    self.fail(err);
                }
            }
            Command::ReplyForm { asid, id, answers } => {
                if let Err(err) = self.hub.reply_form(&asid, &id, answers).await {
                    self.fail(err);
                }
            }
            Command::LoadProjects { agent } => match self.hub.projects(&agent).await {
                Ok(mut projects) => {
                    mark_missing(&mut projects).await;
                    self.inbox.push(Update::Projects { agent, projects })
                }
                Err(err) => tracing::debug!(%err, "project list unavailable"),
            },
            Command::Compose(command) => self.compose(command).await,
            command @ (Command::ThemeRefresh
            | Command::ThemeInstall(_)
            | Command::ThemeRemove(_)
            | Command::ThemeApply(..)
            | Command::ThemeImportFile(_)
            | Command::ThemeImportUrl(_)) => {
                crate::theme::service::run(command, self.inbox.clone(), self.config.clone()).await;
            }
            Command::RenameSession { asid, title } => {
                let title = title.trim().to_string();
                if title.is_empty() {
                    return;
                }
                match self.hub.rename(&asid, &title).await {
                    Ok(()) => match self.hub.snapshot(&asid).await {
                        Ok(mut snapshot) => {
                            snapshot.info.title = title;
                            self.inbox
                                .push(Update::event(AgentDomainEvent::SessionUpdated {
                                    asid: asid.clone(),
                                    info: Box::new(snapshot.info),
                                    seq: 0,
                                }));
                        }
                        Err(_) => {
                            // Still refresh lists so the new title shows up.
                            self.refresh().await;
                        }
                    },
                    Err(err) => self.fail(err),
                }
            }
            Command::DeleteSession(asid) => match self.hub.delete(&asid).await {
                Ok(()) => {
                    if self
                        .state()
                        .active
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|a| a == &asid)
                    {
                        *self.state().active.lock().unwrap() = None;
                    }
                    self.inbox.push(Update::SessionGone(asid.0));
                }
                Err(err) => self.fail(err),
            },
            Command::ForkSession { asid, message_id } => {
                match self.hub.fork(&asid, message_id.as_deref()).await {
                    Ok(info) => {
                        self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                        self.inbox
                            .push(Update::SessionCreated(Box::new(info), None));
                    }
                    Err(err) => self.fail(err),
                }
            }
            Command::LoadDescendants { asid } => self.descendants(asid).await,
            Command::LoadSavedPermissions { asid } => self.saved_permissions(asid).await,
            Command::ForgetSavedPermission { asid, id } => {
                match self.hub.forget_saved_permission(&asid, &id).await {
                    Ok(()) => self.saved_permissions(asid).await,
                    Err(err) => self.fail(err),
                }
            }
            Command::LoadShells { agent, directory } => {
                self.load_shells(agent, directory).await;
            }
            Command::KillShell {
                agent,
                id,
                directory,
            } => match self.hub.kill_shell(&agent, &id).await {
                Ok(()) => self.load_shells(agent, directory).await,
                Err(err) => self.fail(err),
            },
            Command::BackgroundSession { asid } => {
                if let Err(err) = self.hub.background_session(&asid).await {
                    self.fail(err);
                }
            }
            Command::LoadWorktrees { agent, directory } => {
                self.load_worktrees(agent, directory).await;
            }
            Command::RemoveWorktree {
                agent,
                project,
                worktree,
            } => match self
                .hub
                .remove_worktree(&agent, &project, &worktree, false)
                .await
            {
                Ok(()) => self.load_worktrees(agent, project).await,
                Err(err) => self.fail(err),
            },
            Command::RefreshWorktrees { agent, directory } => {
                match self.hub.refresh_worktrees(&agent, &directory).await {
                    Ok(()) => self.load_worktrees(agent, directory).await,
                    Err(err) => self.fail(err),
                }
            }
            Command::Orchestration {
                computer,
                ticket,
                request,
            } => {
                let target = if matches!(computer.as_str(), "" | "self") {
                    "local"
                } else {
                    computer.as_str()
                };
                let origin_matches = crate::hub::connection_origin()
                    .is_none_or(|(c, _)| c == target || (c == "local" && target == "self"));
                let result = if !origin_matches {
                    Err(crate::hub::OrchestrationError {
                        uncertain: false,
                        message: "Computer is still switching; wait for it to connect".into(),
                    })
                } else {
                    self.hub.orchestration(request).await.map_err(|e| {
                        crate::hub::OrchestrationError {
                            uncertain: !matches!(
                                &e,
                                HubError::Agent(
                                    AgentError::InvalidRequest(_)
                                        | AgentError::Unsupported(_)
                                        | AgentError::NotAvailable(_)
                                )
                            ),
                            message: e.to_string(),
                        }
                    })
                };
                self.inbox.push(Update::Orchestration {
                    computer,
                    ticket,
                    result,
                });
            }
            Command::RefreshTasks => self.refresh_tasks().await,
            Command::UnpairComputer {
                computer,
                request,
                gateway,
            } => {
                let result = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    crate::hub::RemoteHub::revoke_own_login(gateway),
                )
                .await
                {
                    Ok(outcome) => outcome.map_err(|err| err.to_string()),
                    Err(_) => Err("Gateway timed out".into()),
                };
                self.inbox.push(Update::Ui(
                    crate::model::inbox::UiAction::ComputerUnpaired {
                        computer,
                        request,
                        result,
                    },
                ));
            }
            Command::RevokeLocalPairing(id) => {
                let result = crate::platform::paired_devices::revoke(&id)
                    .await
                    .map_err(|e| e.to_string());
                self.inbox.push(Update::Ui(
                    crate::model::inbox::UiAction::LocalPairingRevoked { id, result },
                ));
            }
            Command::ReadLocalInvite(id) => {
                let invite = crate::platform::pairing_invite::load().await.ok();
                self.inbox
                    .push(Update::Ui(crate::model::inbox::UiAction::LocalInvite(
                        id, invite,
                    )));
            }
            Command::CheckDesktopUpdate => {
                let status = crate::platform::update::check().await;
                self.inbox
                    .push(Update::Ui(crate::model::inbox::UiAction::DesktopUpdate(
                        status,
                    )));
            }
            Command::RefreshLocalPairings => {
                let devices = crate::platform::paired_devices::load().await;
                self.inbox
                    .push(Update::Ui(crate::model::inbox::UiAction::LocalPairings(
                        devices,
                    )));
            }
            Command::ScanStorage => {
                if let Ok(report) = tokio::task::spawn_blocking(super::storage::scan).await {
                    self.inbox.push(Update::Storage(report));
                }
            }
            Command::ClearStorage => {
                let cleared = tokio::task::spawn_blocking(|| {
                    super::storage::clear_caches();
                    super::storage::scan()
                })
                .await;
                if let Ok(report) = cleared {
                    self.inbox.push(Update::Storage(report));
                }
            }
            Command::Voice(start) => {
                if let Some(control) = start.control.lock().unwrap().take() {
                    super::voice::spawn(
                        start.config.clone(),
                        start.meter.clone(),
                        self.inbox.clone(),
                        control,
                    );
                }
            }
            Command::CreateTask { goal, agent, cwd } => {
                let goal = goal.trim().to_string();
                if goal.is_empty() {
                    return;
                }
                match self
                    .hub
                    .create_task(&goal, agent.as_deref(), cwd.as_deref(), None)
                    .await
                {
                    Ok(task) => {
                        tracing::info!(task = %task.id, state = %task.state, "task delegated");
                        self.refresh_tasks().await;
                    }
                    Err(err) => self.fail(err),
                }
            }
            Command::CancelTask { id } => match self.hub.cancel_task(&id).await {
                Ok(()) => self.refresh_tasks().await,
                Err(err) => self.fail(err),
            },
            Command::CompleteTask { id } => match self.hub.complete_task(&id).await {
                Ok(()) => self.refresh_tasks().await,
                Err(err) => self.fail(err),
            },
            Command::TaskHistory { id } => {
                match tokio::time::timeout(LIST_TIMEOUT, self.hub.task_history(&id)).await {
                    Ok(Ok(steps)) => self.inbox.push(Update::TaskHistory { id, steps }),
                    Ok(Err(err)) => tracing::debug!(task = %id, %err, "task history unavailable"),
                    Err(_) => tracing::debug!(task = %id, "task history timed out"),
                }
            }
            Command::GitCommit {
                cwd,
                message,
                push,
                asid,
            } => {
                let inbox = self.inbox.clone();
                let cwd_clone = cwd.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let path = PathBuf::from(&cwd_clone);
                    match crate::model::git_ops::git_commit(&path, &message) {
                        Ok(summary) => {
                            if push {
                                match crate::model::git_ops::git_push(&path) {
                                    Ok(_) => {
                                        inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                            title: "Committed and pushed".into(),
                                            detail: Some(summary),
                                        }));
                                    }
                                    Err(why) => {
                                        inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                            title: "Committed, but push failed".into(),
                                            detail: Some(why),
                                        }));
                                    }
                                }
                            } else {
                                inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                    title: "Committed changes".into(),
                                    detail: Some(summary),
                                }));
                            }
                        }
                        Err(why) => {
                            inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                title: "Git commit failed".into(),
                                detail: Some(why),
                            }));
                        }
                    }
                })
                .await;
                if let Some(asid) = asid {
                    crate::hub::spawn_connected(self.clone().diff(asid));
                }
                let path = PathBuf::from(&cwd);
                let info =
                    tokio::task::spawn_blocking(move || crate::model::compose::branch_info(&path))
                        .await
                        .unwrap_or_default();
                self.inbox.push(Update::Composer(Box::new(
                    super::compose::ComposerUpdate::Branch { cwd, info },
                )));
            }
            Command::GitPush { cwd, asid } => {
                let inbox = self.inbox.clone();
                let cwd_clone = cwd.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let path = PathBuf::from(&cwd_clone);
                    match crate::model::git_ops::git_push(&path) {
                        Ok(msg) => {
                            inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                title: "Pushed to remote".into(),
                                detail: (!msg.trim().is_empty()).then_some(msg),
                            }));
                        }
                        Err(why) => {
                            inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                                title: "Git push failed".into(),
                                detail: Some(why),
                            }));
                        }
                    }
                })
                .await;
                if let Some(asid) = asid {
                    crate::hub::spawn_connected(self.clone().diff(asid));
                }
                let path = PathBuf::from(&cwd);
                let info =
                    tokio::task::spawn_blocking(move || crate::model::compose::branch_info(&path))
                        .await
                        .unwrap_or_default();
                self.inbox.push(Update::Composer(Box::new(
                    super::compose::ComposerUpdate::Branch { cwd, info },
                )));
            }
            Command::MergeTaskWorktree {
                task_id,
                base_dir,
                branch,
                worktree_dir,
            } => {
                match self
                    .hub
                    .merge_task_worktree(&task_id, &base_dir, &branch, worktree_dir.as_deref())
                    .await
                {
                    Ok(msg) => {
                        self.inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                            title: format!("Merged branch {branch}"),
                            detail: (!msg.trim().is_empty()).then_some(msg),
                        }));
                        self.refresh_tasks().await;
                    }
                    Err(err) => {
                        self.inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                            title: format!("Merge failed: {branch}"),
                            detail: Some(err.to_string()),
                        }));
                    }
                }
            }
            Command::CleanTaskWorktree {
                task_id,
                base_dir,
                worktree_dir,
                branch,
            } => {
                match self
                    .hub
                    .clean_task_worktree(
                        &task_id,
                        &base_dir,
                        worktree_dir.as_deref(),
                        branch.as_deref(),
                    )
                    .await
                {
                    Ok(()) => {
                        self.inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                            title: "Worktree cleaned".into(),
                            detail: Some(format!("Removed task {task_id} worktree")),
                        }));
                        self.refresh_tasks().await;
                    }
                    Err(err) => {
                        self.inbox.push(Update::Ui(super::inbox::UiAction::Toast {
                            title: "Failed to clean worktree".into(),
                            detail: Some(err.to_string()),
                        }));
                    }
                }
            }
            Command::PluginPackage(request, op, reserved) => {
                let inbox = self.inbox.clone();
                tokio::task::spawn_blocking(move || {
                    use crate::diagnostics::{Attempt, EventKind, Outcome};
                    use crate::plugin::packages::Operation;
                    let (action, source) = match &op {
                        Operation::Install(_) => (EventKind::PluginInstall, "Desktop"),
                        Operation::Update(id) => (EventKind::PluginUpdate, id.as_str()),
                        Operation::Remove(id) => (EventKind::PluginRemove, id.as_str()),
                    };
                    let mut attempt = Attempt::new(action, source, None);
                    let result =
                        crate::plugin::packages::apply_with_progress(op, &reserved, |stage| {
                            inbox.push(Update::PluginPackageProgress(request, stage))
                        });
                    if let Ok(id) = &result {
                        attempt.set_source(id);
                    }
                    let event =
                        attempt.finish(result.as_ref().err().map_or(Outcome::Completed, |err| {
                            err.downcast_ref::<Outcome>()
                                .copied()
                                .unwrap_or(Outcome::PackageFailed)
                        }));
                    inbox.push(Update::PluginPackage(
                        request,
                        event,
                        result.map_err(|err| format!("{err:#}")),
                    ));
                });
            }
            Command::ProbeRuntimes => {
                let inbox = self.inbox.clone();
                tokio::task::spawn_blocking(move || {
                    let statuses = muqun_gateway::platform::runtimes::probe_all();
                    inbox.push(Update::Runtimes(statuses));
                });
            }
            Command::InstallManagedRuntime(kind) => {
                let inbox = self.inbox.clone();
                crate::hub::spawn_connected(async move {
                    let attempt = crate::diagnostics::Attempt::new(
                        crate::diagnostics::EventKind::RuntimeInstall,
                        kind.id(),
                        None,
                    );
                    let result =
                        muqun_gateway::platform::runtimes::install_managed(kind, None, None)
                            .await
                            .map_err(|e| format!("{e:#}"));
                    let event = attempt.finish(if result.is_ok() {
                        crate::diagnostics::Outcome::Completed
                    } else {
                        crate::diagnostics::Outcome::RuntimeInstallFailed
                    });
                    inbox.push(Update::RuntimeInstalled {
                        kind,
                        result,
                        event,
                    });
                    let statuses = muqun_gateway::platform::runtimes::probe_all();
                    inbox.push(Update::Runtimes(statuses));
                });
            }
            Command::MaoPrepareReview {
                run_id,
                coder_asid,
                cwd,
                base_commit,
            } => {
                let hub = self.hub.clone();
                let inbox = self.inbox.clone();
                crate::hub::spawn_connected(async move {
                    let mut summary = String::new();
                    let mut changed_files: Vec<(String, usize, usize)> = Vec::new();
                    let allow_local_git = hub.mode() == crate::hub::HubMode::Standalone
                        || crate::hub::connection_origin()
                            .is_none_or(|(c, _)| c == "local" || c == "self");
                    let has_local_git = allow_local_git
                        && !cwd.trim().is_empty()
                        && std::path::Path::new(cwd.trim()).join(".git").exists();
                    if (base_commit.is_none() || !has_local_git) && !coder_asid.starts_with("new-")
                    {
                        if let Ok(Ok(files)) = tokio::time::timeout(
                            LIST_TIMEOUT,
                            hub.vcs_diff(&AgentSessionId(coder_asid.clone())),
                        )
                        .await
                        {
                            if !files.is_empty() {
                                summary.push_str("Changed files (from session diff):\n");
                                for f in files.iter().take(32) {
                                    changed_files.push((f.path.clone(), f.additions, f.deletions));
                                    summary.push_str(&format!(
                                        "- {} (+{} -{})\n",
                                        f.path, f.additions, f.deletions
                                    ));
                                }
                                if files.len() > 32 {
                                    summary.push_str(&format!(
                                        "... and {} more file(s)\n",
                                        files.len() - 32
                                    ));
                                }
                                let mut patches = String::new();
                                for f in &files {
                                    if f.patch.trim().is_empty() {
                                        continue;
                                    }
                                    let entry = format!("\n--- {}\n{}", f.path, f.patch);
                                    if patches.len() + entry.len() > 10_000 {
                                        break;
                                    }
                                    patches.push_str(&entry);
                                }
                                if !patches.is_empty() {
                                    summary.push_str("\nDiff excerpt:\n");
                                    summary.push_str(&patches);
                                }
                            }
                        }
                    }

                    if summary.is_empty() && has_local_git {
                        let cwd_clone = cwd.trim().to_string();
                        if let Ok((local_diff, local_files)) =
                            tokio::task::spawn_blocking(move || {
                                let run_git = |args: &[&str]| -> String {
                                    std::process::Command::new("git")
                                        .arg("-C")
                                        .arg(&cwd_clone)
                                        .args(args)
                                        .output()
                                        .ok()
                                        .filter(|out| out.status.success())
                                        .map(|out| {
                                            String::from_utf8_lossy(&out.stdout).into_owned()
                                        })
                                        .unwrap_or_default()
                                };
                                let mut numstat_args = vec!["diff", "--numstat"];
                                let mut stat_args = vec!["diff", "--stat"];
                                let mut raw_args = vec!["diff", "--no-color"];
                                if let Some(ref rev) = base_commit {
                                    numstat_args.push(rev);
                                    stat_args.push(rev);
                                    raw_args.push(rev);
                                }
                                let status = run_git(&["status", "--porcelain"]);
                                let stat = run_git(&stat_args);
                                let numstat = run_git(&numstat_args);
                                let raw = run_git(&raw_args);
                                let mut parsed_files: Vec<(String, usize, usize)> = Vec::new();
                                for line in numstat.lines().take(32) {
                                    let mut parts = line.split('\t');
                                    if let (Some(a), Some(d), Some(path)) =
                                        (parts.next(), parts.next(), parts.next())
                                    {
                                        let add = a.trim().parse::<usize>().unwrap_or(0);
                                        let del = d.trim().parse::<usize>().unwrap_or(0);
                                        parsed_files.push((path.trim().to_string(), add, del));
                                    }
                                }
                                let mut untracked_notes = Vec::new();
                                for line in status.lines().take(32) {
                                    if let Some(rest) = line.strip_prefix("?? ") {
                                        let clean = rest.trim().trim_matches('"');
                                        if !clean.is_empty()
                                            && !parsed_files.iter().any(|(p, _, _)| p == clean)
                                        {
                                            let full = std::path::Path::new(&cwd_clone).join(clean);
                                            let lines = std::fs::metadata(&full)
                                                .ok()
                                                .filter(|m| m.is_file() && m.len() <= 131_072)
                                                .and_then(|_| std::fs::read_to_string(&full).ok())
                                                .map(|body| body.lines().count())
                                                .unwrap_or(0);
                                            parsed_files.push((clean.to_string(), lines, 0));
                                            untracked_notes.push(format!(
                                                "- {clean} (new file, +{lines} lines)"
                                            ));
                                        }
                                    }
                                }
                                if parsed_files.is_empty() {
                                    for line in status.lines().take(32) {
                                        if line.len() >= 4 {
                                            let p = line[3..].trim().trim_matches('"');
                                            if !p.is_empty() {
                                                parsed_files.push((p.to_string(), 0, 0));
                                            }
                                        }
                                    }
                                }
                                let mut out = String::new();
                                if !status.trim().is_empty() {
                                    out.push_str("Git status:\n");
                                    out.push_str(status.trim());
                                    out.push('\n');
                                }
                                if !stat.trim().is_empty() {
                                    out.push_str("\nGit diff --stat:\n");
                                    out.push_str(stat.trim());
                                    out.push('\n');
                                }
                                if !untracked_notes.is_empty() {
                                    out.push_str("\nNew untracked files:\n");
                                    out.push_str(&untracked_notes.join("\n"));
                                    out.push('\n');
                                }
                                if !raw.trim().is_empty() {
                                    let max_bytes = 10_000;
                                    let excerpt = if raw.len() <= max_bytes {
                                        raw.as_str()
                                    } else {
                                        let mut cut = max_bytes;
                                        while cut > 0 && !raw.is_char_boundary(cut) {
                                            cut -= 1;
                                        }
                                        &raw[..cut]
                                    };
                                    out.push_str("\nGit diff excerpt:\n");
                                    out.push_str(excerpt);
                                }
                                (out, parsed_files)
                            })
                            .await
                        {
                            summary = local_diff;
                            changed_files = local_files;
                        }
                    }

                    inbox.push(Update::MaoReviewReady {
                        run_id,
                        diff_summary: summary,
                        files: changed_files,
                    });
                });
            }
            Command::CompleteDirectory { .. } | Command::Edit(_) => {
                unreachable!("handled in the command loop")
            }
        }
    }

    async fn compose(self: Arc<Self>, command: ComposeCommand) {
        let ComposeCommand::Submit {
            token,
            target,
            deliver,
            attachments,
            delivery,
        } = command
        else {
            let resync = match &command {
                ComposeCommand::SwitchModel { asid, .. }
                | ComposeCommand::SwitchMode { asid, .. } => Some(asid.clone()),
                _ => None,
            };
            if let Some(err) = compose::run(command, self.hub.clone(), self.inbox.clone()).await {
                self.fail(err);
                // Roll back the optimistic header after a failed switch.
                if let Some(asid) = resync {
                    self.open(&asid).await;
                }
            }
            return;
        };
        let asid = match target {
            Target::Session(asid) => asid,
            Target::New {
                agent,
                cwd,
                model,
                mode,
                worktree,
            } => {
                let hint = match &deliver {
                    Deliver::Prompt(text) => text.as_str(),
                    Deliver::Command { name, .. } | Deliver::Skill { name, .. } => name.as_str(),
                };
                let cwd = if worktree {
                    match compose::ensure_worktree(&cwd, hint) {
                        Ok(path) => path,
                        Err(err) => {
                            self.fail(err);
                            self.inbox.push(Update::Composer(Box::new(
                                ComposerUpdate::Submitted { token, ok: false },
                            )));
                            return;
                        }
                    }
                } else {
                    cwd
                };
                match self.hub.create(&agent, &cwd).await {
                    Ok(info) => {
                        let asid = info.asid.clone();
                        self.inbox
                            .push(Update::SessionCreated(Box::new(info), Some(token)));
                        // The UI opens the canonical session only if its pending pane is still visible.
                        self.config.change(|config| {
                            config.general.last_agent = Some(agent.clone());
                            if crate::hub::connection_origin()
                                .is_none_or(|(computer, _)| computer == "local")
                            {
                                config.general.last_cwd.insert(agent.clone(), cwd);
                            }
                        });
                        self.edited();
                        // The choices on the pills are the new session's.
                        if let Some(model) = model {
                            match self.hub.switch_model(&asid, &model).await {
                                Ok(()) => self.inbox.push(Update::Composer(Box::new(
                                    ComposerUpdate::SessionCatalogChanged(asid.clone()),
                                ))),
                                Err(err) => self.fail(err),
                            }
                        }
                        if let Some(mode) = mode {
                            if let Err(err) = self.hub.switch_mode(&asid, &mode).await {
                                self.fail(err);
                            }
                        }
                        asid
                    }
                    Err(err) => {
                        self.fail(err);
                        self.inbox
                            .push(Update::Composer(Box::new(ComposerUpdate::Submitted {
                                token,
                                ok: false,
                            })));
                        return;
                    }
                }
            }
        };
        let result = compose::deliver(
            self.hub.as_ref(),
            &asid,
            &deliver,
            &attachments,
            delivery.as_deref(),
        )
        .await;
        let ok = result.is_ok();
        if let Err(err) = result {
            self.fail(err);
        }
        self.inbox
            .push(Update::Composer(Box::new(ComposerUpdate::Submitted {
                token,
                ok,
            })));
    }

    /// Switch active computer target: reconnect or swap the hub and reload data.
    async fn switch_computer(self: Arc<Self>, target: String) {
        let target = match target.as_str() {
            "" | "self" => "local".to_string(),
            _ => target,
        };
        if target == "all" {
            return;
        }
        if self.connected.load(Ordering::Acquire)
            && self.connection.lock().unwrap().computer == target
            && !(target == "local" && self.hub.mode() == crate::hub::HubMode::Standalone)
        {
            return;
        }
        self.connected.store(false, Ordering::Release);
        for task in self.subscriptions.lock().unwrap().drain(..) {
            task.abort();
        }
        let generation = self.connection_generation.fetch_add(1, Ordering::AcqRel) + 1;
        *self.connection.lock().unwrap() = self.hub.connection(target.clone(), generation);
        *self.state.lock().unwrap() = (generation, Arc::new(ConnectionState::default()));
        tracing::info!(computer = %target, "switching active computer hub");

        let cfg = self.config.get();
        let maybe_new_hub: Option<Arc<dyn SessionHub>> = if let Some(hub) = self.fleet.hub(&target)
        {
            Some(hub)
        } else if target == "local" || target == "self" {
            if let Some(gw) = crate::hub::discover_local_gateway().await {
                crate::hub::RemoteHub::start(gw)
                    .ok()
                    .map(|h| h as Arc<dyn SessionHub>)
            } else if let Some(local) = &self.local_hub {
                Some(local.clone())
            } else if let Some(dir) = &self.local_state_dir {
                // Startup restored a remote login and never created an in-process
                // hub. The first switch home must initialize the same local store.
                Some(crate::standalone_hub(crate::runtime_settings(&cfg), dir).await)
            } else {
                None
            }
        } else if let Some(profile) = cfg.computers.get(&target) {
            if let (Some(url), Some(token)) = (&profile.url, &profile.token) {
                if !url.is_empty() && !token.is_empty() {
                    crate::hub::RemoteHub::start(crate::hub::LocalGateway {
                        url: url.clone(),
                        token: token.clone(),
                        device_id: profile.device_id.clone(),
                        transport_key: profile.transport_key.clone(),
                    })
                    .ok()
                    .map(|h| h as Arc<dyn SessionHub>)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(new_hub) = maybe_new_hub {
            if self.fleet.hub(&target).is_none() {
                self.fleet.insert(
                    target.clone(),
                    new_hub.clone(),
                    self.inbox.clone(),
                    &self.config,
                );
            }
            self.hub.switch(new_hub);
            let context = self.hub.connection(target.clone(), generation);
            *self.connection.lock().unwrap() = context.clone();
            self.connected.store(true, Ordering::Release);
            self.start_subscriptions();
            // The barrier covers swapping the backend and subscriptions. Listing
            // agents must not hold queued session opens behind a slow computer.
            tokio::spawn(context.scope(self.refresh()));
        } else if target != "all" {
            let context = self.connection.lock().unwrap().clone();
            context
                .scope(async {
                    self.inbox.push(Update::Error(UiError {
                        message: format!("Computer '{target}' is not configured or unpaired"),
                        detail: Some("Check computer connection in Settings".into()),
                        unreachable: true,
                    }));
                })
                .await;
        }
    }

    /// Agents and every attached agent's sessions. Each agent's list is
    /// delivered on its own as it arrives, so a slow one never holds the
    /// others back. Starts are staggered so ten agents listing at once do
    /// not spike the first second of startup.
    async fn refresh(self: Arc<Self>) {
        {
            let state = self.state();
            let mut at = state.refreshed_at.lock().unwrap();
            if at.is_some_and(|t| t.elapsed() < REFRESH_DEDUPE) {
                return;
            }
            *at = Some(std::time::Instant::now());
        }
        crate::hub::spawn_connected(self.clone().refresh_tasks());
        crate::hub::spawn_connected(self.clone().refresh_peers());
        match self.hub.agents().await {
            Ok(agents) => self.inbox.push(Update::Agents(agents)),
            Err(err) => {
                tracing::warn!(%err, "agents list failed during refresh");
                self.inbox.push(Update::Error(UiError {
                    message: format!("Could not reach computer: {err}"),
                    detail: Some("Connection failed or authorization was refused".into()),
                    unreachable: true,
                }));
            }
        }
        let attached = self.hub.attached().await.unwrap_or_default();
        self.spawn_lists(attached).await;
    }

    /// The delegated task list and the default folder. A gateway without
    /// task routes (or an unreachable one) leaves the list as it was.
    async fn refresh_tasks(self: Arc<Self>) {
        if self.state().tasks_reading.swap(true, Ordering::AcqRel) {
            return;
        }
        self.clone().read_tasks().await;
        self.state().tasks_reading.store(false, Ordering::Release);
    }

    async fn refresh_peers(self: Arc<Self>) {
        if let Ok(peers) = self.hub.list_peers().await {
            self.inbox.push(Update::Composer(Box::new(
                crate::model::compose::ComposerUpdate::Peers { peers },
            )));
        }
    }

    async fn read_tasks(self: Arc<Self>) {
        let list = tokio::time::timeout(LIST_TIMEOUT, self.hub.list_tasks(None, None));
        match list.await {
            Ok(Ok(rows)) => {
                let default_cwd = match self.hub.task_defaults().await {
                    Ok(defaults) => defaults.default_cwd,
                    Err(err) => {
                        tracing::debug!(%err, "task defaults unavailable");
                        None
                    }
                };
                self.inbox.push(Update::Tasks { rows, default_cwd });
            }
            Ok(Err(err)) => {
                tracing::warn!(%err, "task list unavailable");
                self.inbox.push(Update::TasksFailed(err.to_string()));
            }
            Err(_) => {
                tracing::warn!("task list timed out");
                self.inbox.push(Update::TasksFailed("timed out".into()));
            }
        }
    }

    /// The sessions below `asid` for the sub-agent panel. A failure leaves
    /// the panel as it was.
    async fn descendants(self: Arc<Self>, asid: AgentSessionId) {
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.descendants(&asid)).await {
            Ok(Ok(sessions)) => self.inbox.push(Update::Descendants {
                asid: asid.0,
                sessions,
            }),
            Ok(Err(err)) => tracing::debug!(asid = %asid.0, %err, "descendants unavailable"),
            Err(_) => tracing::debug!(asid = %asid.0, "descendants timed out"),
        }
    }

    /// Remembered "always allow" decisions for the session's project.
    async fn saved_permissions(self: Arc<Self>, asid: AgentSessionId) {
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.list_saved_permissions(&asid)).await {
            Ok(Ok(items)) => self.inbox.push(Update::SavedPermissions {
                asid: asid.0,
                items,
            }),
            Ok(Err(err)) => {
                tracing::debug!(asid = %asid.0, %err, "saved permissions unavailable")
            }
            Err(_) => tracing::debug!(asid = %asid.0, "saved permissions timed out"),
        }
    }

    async fn load_shells(self: Arc<Self>, agent: String, directory: Option<String>) {
        match tokio::time::timeout(
            LIST_TIMEOUT,
            self.hub.list_shells(&agent, directory.as_deref()),
        )
        .await
        {
            Ok(Ok(items)) => self.inbox.push(Update::Shells(items)),
            Ok(Err(err)) => tracing::debug!(%agent, %err, "shells unavailable"),
            Err(_) => tracing::debug!(%agent, "shells timed out"),
        }
    }

    async fn load_worktrees(self: Arc<Self>, agent: String, directory: String) {
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.list_worktrees(&agent, &directory)).await
        {
            Ok(Ok(items)) => self.inbox.push(Update::Worktrees(items)),
            Ok(Err(err)) => tracing::debug!(%agent, %err, "worktrees unavailable"),
            Err(_) => tracing::debug!(%agent, "worktrees timed out"),
        }
    }

    /// Kick off one `list_sessions` per agent, briefly staggered so a cold
    /// attach storm does not spike the first frames.
    async fn spawn_lists(self: &Arc<Self>, agents: Vec<String>) {
        for (i, agent) in agents.into_iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            crate::hub::spawn_connected(self.clone().list_sessions(agent));
        }
    }

    async fn list_sessions(self: Arc<Self>, agent: String) {
        let query = SessionQuery {
            parent_id: Some("null".into()),
            limit: Some(SESSION_LIST_LIMIT),
            ..SessionQuery::default()
        };
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.sessions(&agent, query)).await {
            Ok(Ok(sessions)) => {
                tracing::info!(agent, count = sessions.len(), "sessions listed");
                let dirs = sessions
                    .iter()
                    .filter_map(|s| s.directory.clone())
                    .collect();
                self.inbox.push(Update::AgentSessions {
                    agent: agent.clone(),
                    sessions,
                });
                // Paint the list first; project catalogues and folder resolve
                // continue in the background so a slow agent does not stall Home.
                self.resolve_dirs(dirs, false);
                let core = self.clone();
                crate::hub::spawn_connected(async move {
                    core.known_projects(agent).await;
                });
            }
            Ok(Err(err)) => tracing::warn!(agent, %err, "session list failed"),
            Err(_) => tracing::warn!(agent, "session list timed out"),
        }
    }

    /// Follow the supervisor as it attaches agents after startup, and
    /// discovery as its cold probes finish (up to a minute each): list each
    /// newly attached agent's sessions, and re-read discovery a few times so
    /// "checking…" rows settle. Never blocks a UI request.
    async fn watch_agents(self: Arc<Self>) {
        let started = tokio::time::Instant::now();
        let mut listed: Vec<String> = Vec::new();
        let mut next_discovery = started;
        while started.elapsed() < WATCH_AGENTS_FOR {
            let attached = self.hub.attached().await.unwrap_or_default();
            let fresh: Vec<String> = attached
                .iter()
                .filter(|a| !listed.contains(a))
                .cloned()
                .collect();
            // A refresh that just started lists these already (boot).
            let refreshing = self
                .state()
                .refreshed_at
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() < REFRESH_DEDUPE);
            if !fresh.is_empty() && !refreshing {
                self.spawn_lists(fresh).await;
            }
            listed = attached;
            if tokio::time::Instant::now() >= next_discovery {
                if let Ok(agents) = self.hub.agents().await {
                    self.inbox.push(Update::Agents(agents));
                }
                next_discovery = tokio::time::Instant::now() + DISCOVERY_POLL;
            }
            tokio::time::sleep(ATTACH_POLL).await;
        }
    }

    /// QA: `MUQUN_QA_SWITCH=<asid>,<asid>,…` opens each session in turn, as
    /// a click on its row would, starting `MUQUN_QA_SWITCH_AFTER` seconds
    /// (default 60) after start and `MUQUN_QA_SWITCH_EVERY` (default 8) apart
    /// -- for measuring a session switch without driving the pointer.
    async fn qa_switch(self: Arc<Self>, list: String) {
        let secs = |var: &str, default: u64| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let every = Duration::from_secs(secs("MUQUN_QA_SWITCH_EVERY", 8));
        tokio::time::sleep(Duration::from_secs(secs("MUQUN_QA_SWITCH_AFTER", 60))).await;
        for asid in list.split(',').map(str::trim).filter(|a| !a.is_empty()) {
            tracing::info!(asid, "qa: opening");
            self.inbox
                .push(Update::Ui(super::inbox::UiAction::Open(asid.to_string())));
            tokio::time::sleep(every).await;
        }
    }

    /// Rows on their way to the UI: slimmed, and any picture only the
    /// gateway holds fetched into the media cache (the UI redraws when it
    /// lands).
    fn ready_rows(&self, items: &mut [muqun_gateway::agents::TimelineItem]) {
        super::compact::slim(items);
        let urls = super::media::missing(items);
        if urls.is_empty() {
            return;
        }
        for url in urls {
            if !self.state().fetching.lock().unwrap().insert(url.clone()) {
                continue;
            }
            let (hub, inbox, fetching) = (
                Arc::clone(&self.hub),
                Arc::clone(&self.inbox),
                Arc::clone(&self.state().fetching),
            );
            crate::hub::spawn_connected(async move {
                let landed = match hub.media(&url).await {
                    Ok(bytes) => super::media::target(&url)
                        .is_some_and(|path| super::media::write(&path, &bytes)),
                    Err(err) => {
                        tracing::debug!(%url, %err, "timeline picture unavailable");
                        false
                    }
                };
                fetching.lock().unwrap().remove(&url);
                if landed {
                    inbox.push(Update::MediaReady);
                }
            });
        }
    }

    /// A session's snapshot to the view, and its waiting prompts to the
    /// composer (the view keeps no inbox).
    fn push_snapshot(&self, mut snapshot: muqun_gateway::agents::ports::AgentSessionSnapshot) {
        if !self.connection_is_current() {
            return;
        }
        self.ready_rows(&mut snapshot.timeline);
        self.state()
            .last_seq
            .lock()
            .unwrap()
            .insert(snapshot.info.asid.0.clone(), snapshot.seq);
        self.inbox.push(Update::Snapshot(Box::new(snapshot)));
    }

    async fn open(self: &Arc<Self>, asid: &AgentSessionId) {
        let started = std::time::Instant::now();
        // Warm path: paint retained history now, wake the agent in the
        // background. Cold path still prepares, while the UI already shows
        // the opening placeholder or a desktop-cached timeline.
        match self.hub.peek(asid).await {
            Ok(Some(snapshot)) => {
                if !self.is_open(asid) {
                    return;
                }
                self.resolve_dirs(snapshot.info.directory.iter().cloned().collect(), false);
                self.push_snapshot(snapshot);
                self.watch_viewed(asid);
                tracing::info!(
                    asid = %asid.0,
                    ms = started.elapsed().as_millis() as u64,
                    cached = true,
                    "session open"
                );
                let hub = Arc::clone(&self.hub);
                let wake = asid.clone();
                let core = Arc::clone(self);
                crate::hub::spawn_connected(async move {
                    if let Err(err) = hub.wake(&wake).await {
                        tracing::debug!(asid = %wake.0, %err, "background wake after peek failed");
                    }
                    if core.is_open(&wake) {
                        if let Ok(fresh) = hub.snapshot(&wake).await {
                            if core.is_open(&wake) {
                                core.push_snapshot(fresh);
                            }
                        }
                    }
                });
                crate::hub::spawn_connected(self.clone().diff(asid.clone()));
                crate::hub::spawn_connected(self.clone().descendants(asid.clone()));
            }
            Ok(None) => match self.hub.open(asid).await {
                Ok(snapshot) => {
                    if !self.is_open(asid) {
                        return;
                    }
                    self.resolve_dirs(snapshot.info.directory.iter().cloned().collect(), false);
                    self.push_snapshot(snapshot);
                    self.watch_viewed(asid);
                    tracing::info!(
                        asid = %asid.0,
                        ms = started.elapsed().as_millis() as u64,
                        cached = false,
                        "session open"
                    );
                    crate::hub::spawn_connected(self.clone().diff(asid.clone()));
                    crate::hub::spawn_connected(self.clone().descendants(asid.clone()));
                }
                Err(err) => {
                    if self.is_open(asid) {
                        self.inbox.push(Update::SessionOpenFailed {
                            asid: asid.0.clone(),
                            message: err.to_string(),
                        });
                        self.fail(err);
                    }
                }
            },
            Err(err) => {
                self.inbox.push(Update::SessionOpenFailed {
                    asid: asid.0.clone(),
                    message: err.to_string(),
                });
                self.fail(err);
            }
        }
    }

    /// The session is on screen: the active one or a split tile.
    fn is_open(&self, asid: &AgentSessionId) -> bool {
        if !self.connection_is_current() {
            return false;
        }
        self.state().active.lock().unwrap().as_ref() == Some(asid)
            || self.state().watched.lock().unwrap().contains(asid)
    }

    /// Mark the session read now, and again every [`VIEW_HEARTBEAT`] while it
    /// stays the open one. A newer call ends the heartbeat of an older one.
    fn watch_viewed(self: &Arc<Self>, asid: &AgentSessionId) {
        if !self.connection_is_current() {
            return;
        }
        let beat = self.state().beat.fetch_add(1, Ordering::SeqCst) + 1;
        let core = self.clone();
        let asid = asid.clone();
        crate::hub::spawn_connected(async move {
            loop {
                let open = core.is_open(&asid);
                if !open || core.state().beat.load(Ordering::SeqCst) != beat {
                    return;
                }
                if let Err(err) = core.hub.view(&asid).await {
                    tracing::debug!(asid = %asid.0, %err, "marking the session viewed failed");
                }
                tokio::time::sleep(VIEW_HEARTBEAT).await;
            }
        });
    }

    /// Hand the UI the rows newer than what it has, instead of a whole
    /// snapshot. False when the mirror cannot say (a resync, or an error),
    /// and the snapshot must be read.
    async fn catch_up(&self, asid: &AgentSessionId) -> bool {
        let after = self
            .state()
            .last_seq
            .lock()
            .unwrap()
            .get(&asid.0)
            .copied()
            .unwrap_or(0);
        if after == 0 {
            return false;
        }
        match self.hub.timeline_after(asid, after).await {
            Ok(mut found) if !found.resync => {
                self.ready_rows(&mut found.items);
                if !found.items.is_empty() {
                    self.state()
                        .last_seq
                        .lock()
                        .unwrap()
                        .entry(asid.0.clone())
                        .and_modify(|seq| *seq = (*seq).max(found.latest_seq))
                        .or_insert(found.latest_seq);
                    self.inbox
                        .push(Update::event(AgentDomainEvent::TimelineUpsert {
                            asid: asid.clone(),
                            items: found.items,
                            seq: found.latest_seq,
                        }));
                    true
                } else {
                    found.latest_seq <= after
                }
            }
            Ok(_) => false,
            Err(err) => {
                tracing::debug!(asid = %asid.0, %err, "catch-up failed; reading the snapshot");
                false
            }
        }
    }

    /// The open session's working-tree change count, when the agent can
    /// say; nothing is shown when it cannot.
    async fn diff(self: Arc<Self>, asid: AgentSessionId) {
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.vcs_diff(&asid)).await {
            Ok(Ok(files)) => self.inbox.push(Update::Diff(DiffStat {
                asid: asid.0,
                files: files.len(),
                added: files.iter().map(|f| f.additions).sum(),
                removed: files.iter().map(|f| f.deletions).sum(),
                paths: files
                    .into_iter()
                    .take(48)
                    .map(|f| super::types::DiffFile {
                        path: f.path,
                        added: f.additions,
                        removed: f.deletions,
                        patch: f.patch,
                    })
                    .collect(),
            })),
            Ok(Err(err)) => tracing::debug!(asid = %asid.0, %err, "no diff for the session"),
            Err(_) => tracing::debug!(asid = %asid.0, "diff timed out"),
        }
    }

    /// One agent's own projects, for the sidebar: listed, their folders
    /// checked and resolved like session folders.
    async fn known_projects(self: &Arc<Self>, agent: String) {
        match tokio::time::timeout(LIST_TIMEOUT, self.hub.projects(&agent)).await {
            Ok(Ok(mut projects)) => {
                mark_missing(&mut projects).await;
                let dirs = projects
                    .iter()
                    .filter(|p| !p.missing)
                    .map(|p| p.canonical.clone())
                    .collect();
                self.inbox.push(Update::KnownProjects { agent, projects });
                self.resolve_dirs(dirs, false);
            }
            Ok(Err(err)) => tracing::debug!(agent, %err, "no project list"),
            Err(_) => tracing::debug!(agent, "project list timed out"),
        }
    }

    /// Resolve session folders to their projects (`projects::resolve`) off
    /// the async threads; each folder once unless `again`.
    fn resolve_dirs(self: &Arc<Self>, dirs: Vec<String>, again: bool) {
        if !self.connection_is_current() {
            return;
        }
        let fresh: Vec<String> = {
            let state = self.state();
            let mut resolved = state.resolved.lock().unwrap();
            dirs.into_iter()
                .filter(|d| !d.is_empty())
                .filter(|d| resolved.insert(d.clone()) || again)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        if fresh.is_empty() {
            return;
        }
        let core = self.clone();
        crate::hub::spawn_connected(async move {
            let started = std::time::Instant::now();
            let found = tokio::task::spawn_blocking(move || {
                fresh
                    .into_iter()
                    .map(|dir| {
                        let info = super::projects::resolve(std::path::Path::new(
                            dir.trim_end_matches(['/', '\\']),
                        ));
                        (dir.trim_end_matches(['/', '\\']).to_string(), info)
                    })
                    .collect::<Vec<_>>()
            })
            .await;
            match found {
                Ok(found) => {
                    tracing::debug!(
                        count = found.len(),
                        us = started.elapsed().as_micros() as u64,
                        "session folders resolved"
                    );
                    core.inbox.push(Update::Directories(found));
                }
                Err(err) => tracing::warn!("folder resolution failed: {err}"),
            }
        });
    }

    fn fail(&self, err: HubError) {
        tracing::warn!(%err, "hub call failed");
        let (unreachable, detail) = match &err {
            HubError::AgentUnavailable { agent, .. } if agent == OPENCODE => {
                let binary = self
                    .config
                    .get()
                    .agents
                    .opencode
                    .binary
                    .unwrap_or_else(|| "opencode (from PATH)".into());
                (true, Some(format!("OpenCode binary: {binary}")))
            }
            HubError::AgentUnavailable { .. } => (true, None),
            _ => (false, None),
        };
        self.inbox.push(Update::Error(UiError {
            message: err.to_string(),
            detail,
            unreachable,
        }));
    }

    async fn events(self: Arc<Self>) {
        let mut events = self.hub.events();
        loop {
            match events.recv().await {
                Ok(AgentDomainEvent::Resync { asid, reason }) => {
                    tracing::info!(asid = %asid.0, %reason, "resync requested");
                    self.resync(Some(&asid)).await;
                }
                Ok(event) => {
                    let active = self.state().active.lock().unwrap().clone();
                    let watched = self.state().watched.lock().unwrap().clone();
                    match &event {
                        AgentDomainEvent::SessionUpdated { info, .. } => {
                            self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                        }
                        // A turn ended in the open session: its changes moved.
                        AgentDomainEvent::StatusChanged { asid, status, .. }
                            if (active.as_ref() == Some(asid) || watched.contains(asid))
                                && *status == muqun_gateway::agents::AgentSessionStatus::Idle =>
                        {
                            crate::hub::spawn_connected(self.clone().diff(asid.clone()));
                            crate::hub::spawn_connected(self.clone().descendants(asid.clone()));
                            let core = self.clone();
                            let s_asid = asid.clone();
                            crate::hub::spawn_connected(async move {
                                if !core.catch_up(&s_asid).await {
                                    if let Ok(snapshot) = core.hub.snapshot(&s_asid).await {
                                        if core.is_open(&s_asid) {
                                            core.push_snapshot(snapshot);
                                        }
                                    }
                                }
                            });
                        }
                        _ => {}
                    }
                    if wanted(&event, active.as_ref(), &watched) {
                        if event.seq() != 0 {
                            self.state()
                                .last_seq
                                .lock()
                                .unwrap()
                                .entry(event.asid().0.clone())
                                .and_modify(|seq| *seq = (*seq).max(event.seq()))
                                .or_insert(event.seq());
                        }
                        let mut event = event;
                        if let AgentDomainEvent::TimelineUpsert { items, .. } = &mut event {
                            self.ready_rows(items);
                        }
                        self.inbox.push(Update::event(event));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "event stream lagged; reading the session again");
                    self.resync(None).await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Follow the hub's task changes: any of them (or a lag, or a resync)
    /// re-reads the task list, which stays the one source of the rows.
    async fn task_events(self: Arc<Self>, mut events: broadcast::Receiver<TaskHubEvent>) {
        loop {
            match events.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => self.refresh_tasks_soon(),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    /// Re-read the task list in the background; a burst of events costs at
    /// most one read in flight and one after it.
    fn refresh_tasks_soon(self: &Arc<Self>) {
        if !self.state().retask.begin() {
            return;
        }
        let core = self.clone();
        crate::hub::spawn_connected(async move {
            loop {
                core.clone().refresh_tasks().await;
                if !core.state().retask.again() {
                    break;
                }
            }
        });
    }

    /// Read the active and watched session(s) again when `asid` is open (or unnamed),
    /// else -- or for an unnamed resync, also -- the lists.
    ///
    /// Re-reading never prepares the session: preparing a terminal-held ACP
    /// session starts another transcript follower, and every follower sends
    /// a resync each time the transcript moves, so answering resyncs with
    /// `open` doubled the followers -- and the reads -- every second the agent
    /// worked. Re-reads are also coalesced: a burst of resyncs costs at most
    /// one read in flight and one after it.
    async fn resync(self: &Arc<Self>, asid: Option<&AgentSessionId>) {
        let named = asid.filter(|x| !x.0.is_empty());
        let active = self.state().active.lock().unwrap().clone();
        let watched = self.state().watched.lock().unwrap().clone();
        let target_asids: Vec<AgentSessionId> = match named {
            Some(target) => {
                if active.as_ref() == Some(target) || watched.contains(target) {
                    vec![target.clone()]
                } else {
                    vec![]
                }
            }
            None => {
                let mut list = Vec::new();
                if let Some(ref a) = active {
                    list.push(a.clone());
                }
                for w in &watched {
                    if !list.contains(w) {
                        list.push(w.clone());
                    }
                }
                list
            }
        };
        let reread = !target_asids.is_empty();
        if reread && self.state().reread.begin() {
            let core = self.clone();
            crate::hub::spawn_connected(async move {
                loop {
                    for asid in &target_asids {
                        if !core.is_open(asid) {
                            continue;
                        }
                        if !core.catch_up(asid).await {
                            match core.hub.snapshot(asid).await {
                                Ok(snapshot) => core.push_snapshot(snapshot),
                                Err(err) => {
                                    tracing::debug!(asid = %asid.0, %err, "resync read failed")
                                }
                            }
                        }
                    }
                    if !core.state().reread.again() {
                        break;
                    }
                }
            });
        }
        if (named.is_none() || !reread) && self.state().relist.begin() {
            let core = self.clone();
            crate::hub::spawn_connected(async move {
                loop {
                    core.clone().relist().await;
                    if !core.state().relist.again() {
                        break;
                    }
                }
            });
        }
    }

    /// Agents and every attached agent's sessions, finished before it
    /// returns (unlike [`Self::refresh`], which lists in the background).
    async fn relist(self: Arc<Self>) {
        if let Ok(agents) = self.hub.agents().await {
            self.inbox.push(Update::Agents(agents));
        }
        let attached = self.hub.attached().await.unwrap_or_default();
        let lists: Vec<_> = attached
            .into_iter()
            .map(|agent| crate::hub::spawn_connected(self.clone().list_sessions(agent)))
            .collect();
        for list in lists {
            let _ = list.await;
        }
    }

    async fn complete_directories(
        self: Arc<Self>,
        mut prefixes: watch::Receiver<Option<(crate::hub::ConnectionContext, String)>>,
    ) {
        while prefixes.changed().await.is_ok() {
            // Debounce: wait until the prefix stops changing.
            loop {
                match tokio::time::timeout(COMPLETION_DEBOUNCE, prefixes.changed()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => return,
                    Err(_) => break,
                }
            }
            let Some((connection, prefix)) = prefixes.borrow_and_update().clone() else {
                continue;
            };
            connection
                .scope(async {
                    if !self.connection_is_current() || !self.connected.load(Ordering::Acquire) {
                        return;
                    }
                    if prefix.trim().is_empty() {
                        // Nothing typed: nothing to suggest, and any earlier
                        // suggestions stop being offered.
                        self.inbox.push(Update::Completions {
                            prefix,
                            entries: Vec::new(),
                        });
                        return;
                    }
                    match self.hub.directories(&prefix).await {
                        Ok(found) => self.inbox.push(Update::Completions {
                            prefix,
                            entries: found.directories,
                        }),
                        Err(err) => tracing::debug!(%err, "directory completion failed"),
                    }
                })
                .await;
        }
    }

    fn edit(&self, edit: ConfigEdit) {
        let changed_login = match &edit {
            ConfigEdit::ComputerToken { computer, .. } | ConfigEdit::ForgetComputer(computer) => {
                Some(computer.clone())
            }
            _ => None,
        };
        self.config.change(|config| config.apply(edit));
        if let Some(computer) = changed_login {
            self.fleet.invalidate(&computer);
            if self.connection.lock().unwrap().computer == computer {
                self.connected.store(false, Ordering::Release);
            }
        }
        self.edited();
    }

    fn edited(&self) {
        self.edits.send_modify(|n| *n += 1);
    }

    /// Write the configuration once edits have stood still for
    /// [`SAVE_DEBOUNCE`]: dragging the sidebar or typing in a setting is
    /// many edits and one write.
    async fn save_config(self: Arc<Self>, mut edits: watch::Receiver<u64>) {
        while edits.changed().await.is_ok() {
            loop {
                match tokio::time::timeout(SAVE_DEBOUNCE, edits.changed()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => return,
                    Err(_) => break,
                }
            }
            let config = self.config.clone();
            if let Err(err) = tokio::task::spawn_blocking(move || config.flush()).await {
                tracing::warn!("config save task failed: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::AgentSessionStatus;

    fn id(s: &str) -> AgentSessionId {
        AgentSessionId(s.into())
    }

    fn test_core() -> Arc<Core> {
        let backend = crate::hub::RemoteHub::start(crate::hub::LocalGateway {
            url: "http://127.0.0.1:1".into(),
            token: "test-only".into(),
            device_id: None,
            transport_key: None,
        })
        .unwrap();
        let hub = DynamicHub::new(backend);
        Arc::new(Core {
            connection: Mutex::new(hub.connection("local".into(), 0)),
            local_hub: None,
            local_state_dir: None,
            fleet: Arc::new(super::super::fleet_attention::Fleet::default()),
            connection_generation: AtomicU64::new(0),
            connected: std::sync::atomic::AtomicBool::new(true),
            subscriptions: Mutex::new(Vec::new()),
            hub,
            inbox: Arc::new(Inbox::new()),
            config: ConfigStore::new(PathBuf::new(), DesktopConfig::default()),
            state: Mutex::new((0, Arc::new(ConnectionState::default()))),
            completions: watch::channel(None).0,
            edits: watch::channel(0).0,
        })
    }

    #[tokio::test]
    async fn manual_pairing_uses_verified_identity_and_switches_once_after_saving() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", server.local_addr().unwrap());
        let http = tokio::spawn(async move {
            for _ in 0..2 {
                let (socket, _) = server.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim(), "GET /api/meta HTTP/1.1");
                let mut authorized = false;
                loop {
                    line.clear();
                    assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    authorized |= line
                        .trim()
                        .eq_ignore_ascii_case("Authorization: Bearer fixture");
                }
                assert!(authorized);
                let meta = r#"{"serverId":"lab-id","label":"Lab"}"#;
                socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{meta}", meta.len()).as_bytes()).await.unwrap();
            }
        });
        let core = test_core();
        let (tx, mut commands) = crate::model::commands::channel();
        let mut ui = crate::ui::UiState::new(tx);
        ui.open_manual_pairing();
        ui.pairing_busy = true;
        let verify = |id: &str| Command::PairComputerToken {
            attempt: 0,
            server_id: id.into(),
            url: url.clone(),
            token: "fixture".into(),
        };
        core.clone().run(verify("different-id")).await;
        ui.apply_updates(core.inbox.take());
        assert!(ui
            .pairing_error
            .as_deref()
            .is_some_and(|e| e.contains("mismatch")));
        assert_eq!(ui.current_computer(), "local");
        assert!(ui.cfg.computers.is_empty());

        ui.pairing_busy = true;
        core.clone().run(verify("")).await;
        ui.apply_updates(core.inbox.take());
        http.await.unwrap();
        assert_eq!(ui.current_computer(), "lab-id");
        let mut switched = 0;
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::Edit(edit) => core.edit(edit),
                Command::SwitchComputer(id) => {
                    assert_eq!(id, "lab-id");
                    let saved = core.config.get();
                    let profile = &saved.computers[&id];
                    assert_eq!(profile.url.as_deref(), Some(url.as_str()));
                    assert_eq!(profile.token.as_deref(), Some("fixture"));
                    assert_eq!(profile.name, "Lab");
                    switched += 1;
                }
                _ => {}
            }
        }
        assert_eq!(switched, 1);
        let storage = tempfile::tempdir().unwrap();
        let path = storage.path().join("desktop.json");
        core.config.get().save(&path).unwrap();
        let restored = DesktopConfig::load(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        assert_eq!(restored.current_computer, "lab-id");
        assert!(crate::configured_remote_hub(&restored).unwrap().is_some());

        ui.open_manual_pairing();
        ui.pairing_busy = true;
        ui.pairing_attempt = 2;
        ui.apply_updates(vec![Update::ComputerPairingOutcome {
            attempt: 1,
            url: url.clone(),
            result: Err("late response".into()),
        }]);
        assert!(ui.pairing_error.is_none() && ui.pairing_busy);
    }

    #[tokio::test]
    async fn a_late_reader_cannot_change_the_new_computers_view_or_sequence() {
        let core = test_core();
        let old = core.connection.lock().unwrap().clone();
        let old_state = core.state();
        *old_state.active.lock().unwrap() = Some(id("shared-id"));
        let current = Arc::new(ConnectionState::default());
        *current.active.lock().unwrap() = Some(id("shared-id"));
        current
            .last_seq
            .lock()
            .unwrap()
            .insert("shared-id".into(), 5);
        *core.state.lock().unwrap() = (1, current.clone());
        old.scope(async {
            assert!(!core.is_open(&id("shared-id")));
            // A reader already holding the old state and one resuming after an
            // await both write outside the current computer's state.
            old_state
                .last_seq
                .lock()
                .unwrap()
                .insert("shared-id".into(), 90);
            core.state()
                .last_seq
                .lock()
                .unwrap()
                .insert("shared-id".into(), 100);
            core.resolve_dirs(vec!["/not-on-the-current-computer".into()], false);
            core.watch_viewed(&id("shared-id"));
        })
        .await;
        assert_eq!(current.last_seq.lock().unwrap().get("shared-id"), Some(&5));
        assert!(current.resolved.lock().unwrap().is_empty());
        assert_eq!(current.beat.load(Ordering::SeqCst), 0);
        assert!(core.inbox.is_empty());
    }

    #[tokio::test]
    async fn failed_switch_rejects_open_and_returns_unsent_prompt_to_its_composer() {
        let core = test_core();
        core.clone()
            .switch_computer("missing-computer".into())
            .await;
        let context = core.connection.lock().unwrap().clone();
        context
            .scope(async {
                core.clone()
                    .run(Command::OpenSession(id("old-computers-session")))
                    .await;
                core.clone()
                    .run(Command::Compose(ComposeCommand::Submit {
                        token: 71,
                        target: Target::Session(id("old-computers-session")),
                        deliver: Deliver::Prompt("must not be sent".into()),
                        attachments: Vec::new(),
                        delivery: None,
                    }))
                    .await;
            })
            .await;
        assert!(!core.connected.load(Ordering::Acquire));
        assert!(core.state().active.lock().unwrap().is_none());
        let updates = core.inbox.take();
        let batch = updates
            .into_iter()
            .find_map(|update| match update {
                Update::Computer {
                    computer, updates, ..
                } if computer == "missing-computer" => Some(updates),
                _ => None,
            })
            .expect("errors and receipts belong to the attempted computer");
        assert!(batch.iter().any(|update| matches!(update, Update::SessionOpenFailed { asid, .. } if asid == "old-computers-session")));
        assert!(batch.iter().any(|update| matches!(update, Update::Composer(value) if matches!(**value, ComposerUpdate::Submitted { token: 71, ok: false }))));
    }

    #[test]
    fn a_burst_of_requests_runs_once_and_once_more() {
        let c = Coalesce::default();
        assert!(c.begin(), "the first starts");
        assert!(!c.begin() && !c.begin(), "the rest fold in");
        assert!(c.again(), "one more run for everything asked meanwhile");
        assert!(!c.again(), "then it is done");
        assert!(c.begin(), "and a later request starts again");
    }

    #[test]
    fn only_the_open_session_streams_its_timeline_to_the_ui() {
        let upsert = |asid: &str| AgentDomainEvent::TimelineUpsert {
            asid: id(asid),
            items: vec![],
            seq: 1,
        };
        assert!(wanted(&upsert("open"), Some(&id("open")), &[]));
        assert!(!wanted(&upsert("other"), Some(&id("open")), &[]));
        assert!(!wanted(&upsert("other"), None, &[]));
        assert!(
            wanted(&upsert("other"), Some(&id("open")), &[id("other")]),
            "a split tile's session streams too"
        );
        let status = AgentDomainEvent::StatusChanged {
            asid: id("other"),
            status: AgentSessionStatus::Busy,
            error: None,
            seq: 1,
        };
        assert!(
            wanted(&status, Some(&id("open")), &[]),
            "a row shows status"
        );
        let removed = |asid: &str| AgentDomainEvent::TimelineRemoved {
            asid: id(asid),
            ids: vec!["x".into()],
            seq: 2,
        };
        assert!(wanted(&removed("open"), Some(&id("open")), &[]));
        assert!(!wanted(&removed("other"), Some(&id("open")), &[]));
        let resolved = AgentDomainEvent::FormResolved {
            asid: id("open"),
            form_id: "f".into(),
            reason: None,
            seq: 3,
        };
        assert!(
            wanted(&resolved, Some(&id("open")), &[]),
            "the open session's forms"
        );
    }

    #[test]
    fn split_tiles_all_stream_regardless_of_which_is_focused() {
        let upsert = |asid: &str| AgentDomainEvent::TimelineUpsert {
            asid: id(asid),
            items: vec![],
            seq: 1,
        };
        let tiles = vec![id("tile-a"), id("tile-b")];
        // When tile-a is focused:
        assert!(wanted(&upsert("tile-a"), Some(&id("tile-a")), &tiles));
        assert!(wanted(&upsert("tile-b"), Some(&id("tile-a")), &tiles));
        assert!(!wanted(&upsert("tile-c"), Some(&id("tile-a")), &tiles));

        // When tile-b is focused:
        assert!(wanted(&upsert("tile-a"), Some(&id("tile-b")), &tiles));
        assert!(wanted(&upsert("tile-b"), Some(&id("tile-b")), &tiles));
        assert!(!wanted(&upsert("tile-c"), Some(&id("tile-b")), &tiles));
    }
}
