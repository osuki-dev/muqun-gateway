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
use super::inbox::{Inbox, UiError, Update};
use super::types::{DiffStat, SESSION_LIST_LIMIT};
use crate::config::{ConfigEdit, DesktopConfig};
use crate::hub::{HubError, SessionHub, TaskHubEvent, OPENCODE};

/// How long the typed prefix must stand still before folders are listed.
const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);
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

struct Core {
    hub: Arc<dyn SessionHub>,
    inbox: Arc<Inbox>,
    config: Arc<ConfigStore>,
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
    completions: watch::Sender<Option<String>>,
    /// Bumped on every edit; the saver waits for it to go quiet.
    edits: watch::Sender<u64>,
    /// Session folders already resolved to their projects.
    resolved: Mutex<std::collections::HashSet<String>>,
    /// The newest sequence handed to the UI per session, for catching up
    /// after a resync.
    last_seq: Mutex<HashMap<String, u64>>,
    /// Which view-heartbeat task is current; older ones end at their next tick.
    beat: AtomicU64,
}

/// Start the core side. Returns at once; the tasks live as long as the
/// runtime does.
pub fn spawn(
    hub: Arc<dyn SessionHub>,
    inbox: Arc<Inbox>,
    config: Arc<ConfigStore>,
    commands: CommandReceiver,
) {
    let (completions, completion_rx) = watch::channel(None);
    let (edits, edits_rx) = watch::channel(0);
    let core = Arc::new(Core {
        hub,
        inbox,
        config,
        active: Mutex::new(None),
        watched: Mutex::new(Vec::new()),
        reread: Coalesce::default(),
        relist: Coalesce::default(),
        retask: Coalesce::default(),
        completions,
        edits,
        resolved: Mutex::new(Default::default()),
        last_seq: Mutex::new(HashMap::new()),
        beat: AtomicU64::new(0),
    });
    core.inbox.push(Update::HubMode(core.hub.mode()));
    tokio::spawn(core.clone().events());
    tokio::spawn(super::notify::watch(core.hub.clone(), core.inbox.clone()));
    if let Some(tasks) = core.hub.task_events() {
        tokio::spawn(core.clone().task_events(tasks));
    }
    tokio::spawn(core.clone().complete_directories(completion_rx));
    tokio::spawn(core.clone().save_config(edits_rx));
    tokio::spawn(core.clone().watch_agents());
    if let Ok(list) = std::env::var("MUQUN_QA_SWITCH") {
        tokio::spawn(core.clone().qa_switch(list));
    }
    tokio::spawn(core.commands(commands));
}

impl Core {
    async fn commands(self: Arc<Self>, mut commands: CommandReceiver) {
        while let Some(command) = commands.recv().await {
            let core = self.clone();
            match command {
                Command::CompleteDirectory { prefix } => {
                    let _ = self.completions.send(Some(prefix));
                }
                // In this loop, not a task: edits must apply in the order sent.
                Command::Edit(edit) => self.edit(edit),
                command => {
                    tokio::spawn(async move { core.run(command).await });
                }
            }
        }
        tracing::debug!("the UI closed its command channel");
    }

    async fn run(self: Arc<Self>, command: Command) {
        match command {
            Command::Refresh => self.refresh().await,
            Command::OpenSession(asid) => {
                *self.active.lock().unwrap() = Some(asid.clone());
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
                    let mut watched = self.watched.lock().unwrap();
                    if !watched.contains(&asid) {
                        watched.push(asid.clone());
                    }
                }
                self.open(&asid).await;
            }
            Command::WatchSessions(asids) => {
                *self.watched.lock().unwrap() = asids;
            }
            Command::CreateSession { agent, cwd } => match self.hub.create(&agent, &cwd).await {
                Ok(info) => {
                    let asid = info.asid.clone();
                    self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                    self.inbox.push(Update::SessionCreated(Box::new(info)));
                    *self.active.lock().unwrap() = Some(asid.clone());
                    self.open(&asid).await;
                    self.config.change(|config| {
                        config.general.last_agent = Some(agent.clone());
                        config.general.last_cwd.insert(agent, cwd);
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
                Ok(items) => {
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
                        .active
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|a| a == &asid)
                    {
                        *self.active.lock().unwrap() = None;
                    }
                    self.inbox.push(Update::SessionGone(asid.0));
                }
                Err(err) => self.fail(err),
            },
            Command::ForkSession { asid, message_id } => {
                match self.hub.fork(&asid, message_id.as_deref()).await {
                    Ok(info) => {
                        let new_asid = info.asid.clone();
                        self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                        self.inbox.push(Update::SessionCreated(Box::new(info)));
                        *self.active.lock().unwrap() = Some(new_asid.clone());
                        self.open(&new_asid).await;
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
            Command::RefreshTasks => self.refresh_tasks().await,
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
            Command::Terminals(command) => {
                super::terminals::run(self.hub.clone(), self.inbox.clone(), command).await
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
                        self.inbox.push(Update::SessionCreated(Box::new(info)));
                        *self.active.lock().unwrap() = Some(asid.clone());
                        self.open(&asid).await;
                        self.config.change(|config| {
                            config.general.last_agent = Some(agent.clone());
                            config.general.last_cwd.insert(agent.clone(), cwd);
                        });
                        self.edited();
                        // The choices on the pills are the new session's.
                        if let Some(model) = model {
                            if let Err(err) = self.hub.switch_model(&asid, &model).await {
                                self.fail(err);
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

    /// Agents and every attached agent's sessions. Each agent's list is
    /// delivered on its own as it arrives, so a slow one never holds the
    /// others back. Starts are staggered so ten agents listing at once do
    /// not spike the first second of startup.
    async fn refresh(self: Arc<Self>) {
        tokio::spawn(self.clone().refresh_tasks());
        if let Ok(agents) = self.hub.agents().await {
            self.inbox.push(Update::Agents(agents));
        }
        let attached = self.hub.attached().await.unwrap_or_default();
        self.spawn_lists(attached).await;
    }

    /// The delegated task list and the default folder. A gateway without
    /// task routes (or an unreachable one) leaves the list as it was.
    async fn refresh_tasks(self: Arc<Self>) {
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
            Ok(Err(err)) => tracing::warn!(%err, "task list unavailable"),
            Err(_) => tracing::warn!("task list timed out"),
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
            tokio::spawn(self.clone().list_sessions(agent));
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
                tokio::spawn(async move {
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
            if !fresh.is_empty() {
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

    /// A session's snapshot to the view, and its waiting prompts to the
    /// composer (the view keeps no inbox).
    fn push_snapshot(&self, snapshot: muqun_gateway::agents::ports::AgentSessionSnapshot) {
        self.last_seq
            .lock()
            .unwrap()
            .insert(snapshot.info.asid.0.clone(), snapshot.seq);
        super::compose::push_inbox(&self.inbox, &snapshot.info.asid.0, &snapshot.inbox);
        self.inbox.push(Update::Snapshot(Box::new(snapshot)));
    }

    async fn open(self: &Arc<Self>, asid: &AgentSessionId) {
        let started = std::time::Instant::now();
        // Warm path: paint retained history now, wake the agent in the
        // background. Cold path still prepares, while the UI already shows
        // the opening placeholder or a desktop-cached timeline.
        match self.hub.peek(asid).await {
            Ok(Some(snapshot)) => {
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
                tokio::spawn(async move {
                    if let Err(err) = hub.open(&wake).await {
                        tracing::debug!(asid = %wake.0, %err, "background wake after peek failed");
                    }
                });
                tokio::spawn(self.clone().diff(asid.clone()));
                tokio::spawn(self.clone().descendants(asid.clone()));
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
                    tokio::spawn(self.clone().diff(asid.clone()));
                    tokio::spawn(self.clone().descendants(asid.clone()));
                }
                Err(err) => {
                    if self.is_open(asid) {
                        self.fail(err);
                    }
                }
            },
            Err(err) => self.fail(err),
        }
    }

    /// The session is on screen: the active one or a split tile.
    fn is_open(&self, asid: &AgentSessionId) -> bool {
        self.active.lock().unwrap().as_ref() == Some(asid)
            || self.watched.lock().unwrap().contains(asid)
    }

    /// Mark the session read now, and again every [`VIEW_HEARTBEAT`] while it
    /// stays the open one. A newer call ends the heartbeat of an older one.
    fn watch_viewed(self: &Arc<Self>, asid: &AgentSessionId) {
        let beat = self.beat.fetch_add(1, Ordering::SeqCst) + 1;
        let core = self.clone();
        let asid = asid.clone();
        tokio::spawn(async move {
            loop {
                let open = core.is_open(&asid);
                if !open || core.beat.load(Ordering::SeqCst) != beat {
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
            Ok(found) if !found.resync => {
                if !found.items.is_empty() {
                    self.last_seq
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
                }
                true
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
        let fresh: Vec<String> = {
            let mut resolved = self.resolved.lock().unwrap();
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
        tokio::spawn(async move {
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
                Ok(AgentDomainEvent::InboxChanged { asid, items, .. }) => {
                    if self.is_open(&asid) {
                        super::compose::push_inbox(&self.inbox, &asid.0, &items);
                    }
                }
                Ok(event) => {
                    let active = self.active.lock().unwrap().clone();
                    let watched = self.watched.lock().unwrap().clone();
                    match &event {
                        AgentDomainEvent::SessionUpdated { info, .. } => {
                            self.resolve_dirs(info.directory.iter().cloned().collect(), false);
                        }
                        // A turn ended in the open session: its changes moved.
                        AgentDomainEvent::StatusChanged { asid, status, .. }
                            if (active.as_ref() == Some(asid) || watched.contains(asid))
                                && *status == muqun_gateway::agents::AgentSessionStatus::Idle =>
                        {
                            tokio::spawn(self.clone().diff(asid.clone()));
                            tokio::spawn(self.clone().descendants(asid.clone()));
                        }
                        _ => {}
                    }
                    if wanted(&event, active.as_ref(), &watched) {
                        if event.seq() != 0 {
                            self.last_seq
                                .lock()
                                .unwrap()
                                .entry(event.asid().0.clone())
                                .and_modify(|seq| *seq = (*seq).max(event.seq()))
                                .or_insert(event.seq());
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
        if !self.retask.begin() {
            return;
        }
        let core = self.clone();
        tokio::spawn(async move {
            loop {
                core.clone().refresh_tasks().await;
                if !core.retask.again() {
                    break;
                }
            }
        });
    }

    /// Read the active session again when `asid` is it (or unnamed), else
    /// -- or for an unnamed resync, also -- the lists.
    ///
    /// Re-reading never prepares the session: preparing a terminal-held ACP
    /// session starts another transcript follower, and every follower sends
    /// a resync each time the transcript moves, so answering resyncs with
    /// `open` doubled the followers -- and the reads -- every second the agent
    /// worked. Re-reads are also coalesced: a burst of resyncs costs at most
    /// one read in flight and one after it.
    async fn resync(self: &Arc<Self>, asid: Option<&AgentSessionId>) {
        let named = asid.filter(|x| !x.0.is_empty());
        let active = self.active.lock().unwrap().clone();
        let reread = active
            .as_ref()
            .is_some_and(|a| named.is_none_or(|x| x == a));
        if reread && self.reread.begin() {
            let core = self.clone();
            tokio::spawn(async move {
                loop {
                    let active = core.active.lock().unwrap().clone();
                    if let Some(active) = active {
                        if !core.catch_up(&active).await {
                            match core.hub.snapshot(&active).await {
                                Ok(snapshot) => core.push_snapshot(snapshot),
                                Err(err) => tracing::debug!(%err, "resync read failed"),
                            }
                        }
                    }
                    if !core.reread.again() {
                        break;
                    }
                }
            });
        }
        if (named.is_none() || !reread) && self.relist.begin() {
            let core = self.clone();
            tokio::spawn(async move {
                loop {
                    core.clone().relist().await;
                    if !core.relist.again() {
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
            .map(|agent| tokio::spawn(self.clone().list_sessions(agent)))
            .collect();
        for list in lists {
            let _ = list.await;
        }
    }

    async fn complete_directories(self: Arc<Self>, mut prefixes: watch::Receiver<Option<String>>) {
        while prefixes.changed().await.is_ok() {
            // Debounce: wait until the prefix stops changing.
            loop {
                match tokio::time::timeout(COMPLETION_DEBOUNCE, prefixes.changed()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(_)) => return,
                    Err(_) => break,
                }
            }
            let Some(prefix) = prefixes.borrow_and_update().clone() else {
                continue;
            };
            if prefix.trim().is_empty() {
                // Nothing typed: nothing to suggest, and any earlier
                // suggestions stop being offered.
                self.inbox.push(Update::Completions {
                    prefix,
                    entries: Vec::new(),
                });
                continue;
            }
            match self.hub.directories(&prefix).await {
                Ok(found) => self.inbox.push(Update::Completions {
                    prefix,
                    entries: found.directories,
                }),
                Err(err) => tracing::debug!(%err, "directory completion failed"),
            }
        }
    }

    fn edit(&self, edit: ConfigEdit) {
        self.config.change(|config| config.apply(edit));
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
}
