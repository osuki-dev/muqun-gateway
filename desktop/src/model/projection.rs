//! The core side of the boundary: runs on the tokio runtime, turns
//! [`Command`]s into hub calls and hub events into inbox [`Update`]s.
//!
//! Nothing here touches Blinc except through the inbox's wake-up.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use muqun_gateway::agents::ports::AgentError;
use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId, SessionQuery};
use tokio::sync::{broadcast, watch};

use super::commands::{Command, CommandReceiver};
use super::compose::{self, ComposeCommand, ComposerUpdate, Target};
use super::inbox::{Inbox, UiError, Update};
use super::view::SESSION_LIST_LIMIT;
use crate::config::{ConfigEdit, DesktopConfig};
use crate::hub::{HubError, SessionHub, OPENCODE};

/// How long the typed prefix must stand still before folders are listed.
const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(150);
/// How long the configuration must stand still before it is written.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
/// One agent's session list, at most.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after startup newly attached agents are looked for: the gateway
/// probes cold agents for up to `DISCOVERY_TIMEOUT` (60 s).
const WATCH_AGENTS_FOR: Duration = Duration::from_secs(90);
const ATTACH_POLL: Duration = Duration::from_secs(2);
const DISCOVERY_POLL: Duration = Duration::from_secs(5);

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
fn wanted(event: &AgentDomainEvent, active: Option<&AgentSessionId>) -> bool {
    match event {
        AgentDomainEvent::SessionUpdated { .. }
        | AgentDomainEvent::StatusChanged { .. }
        | AgentDomainEvent::PermissionPending { .. }
        | AgentDomainEvent::PermissionResolved { .. }
        | AgentDomainEvent::Resync { .. } => true,
        other => active.is_some_and(|active| active == other.asid()),
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
    /// Re-reads of the active session's snapshot, after a resync.
    reread: Coalesce,
    /// Re-reads of every agent's list, after a resync.
    relist: Coalesce,
    completions: watch::Sender<Option<String>>,
    /// Bumped on every edit; the saver waits for it to go quiet.
    edits: watch::Sender<u64>,
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
        reread: Coalesce::default(),
        relist: Coalesce::default(),
        completions,
        edits,
    });
    tokio::spawn(core.clone().events());
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
            Command::CreateSession { agent, cwd } => match self.hub.create(&agent, &cwd).await {
                Ok(info) => {
                    let asid = info.asid.clone();
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
            | Command::ThemeImportFile(_)) => {
                crate::theme::service::run(command, self.inbox.clone(), self.config.clone()).await;
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
            if let Some(err) = compose::run(command, self.hub.clone(), self.inbox.clone()).await {
                self.fail(err);
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
            } => match self.hub.create(&agent, &cwd).await {
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
            },
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
    /// others back.
    async fn refresh(self: Arc<Self>) {
        if let Ok(agents) = self.hub.agents().await {
            self.inbox.push(Update::Agents(agents));
        }
        let attached = self.hub.attached().await.unwrap_or_default();
        for agent in attached {
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
                self.inbox.push(Update::AgentSessions { agent, sessions });
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
            for agent in attached.iter().filter(|a| !listed.contains(a)) {
                tokio::spawn(self.clone().list_sessions(agent.clone()));
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
        super::compose::push_inbox(&self.inbox, &snapshot.info.asid.0, &snapshot.inbox);
        self.inbox.push(Update::Snapshot(Box::new(snapshot)));
    }

    async fn open(&self, asid: &AgentSessionId) {
        match self.hub.open(asid).await {
            Ok(snapshot) => self.push_snapshot(snapshot),
            Err(err) => self.fail(err),
        }
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
                    let active = self.active.lock().unwrap().clone();
                    if active.as_ref() == Some(&asid) {
                        super::compose::push_inbox(&self.inbox, &asid.0, &items);
                    }
                }
                Ok(event) => {
                    let active = self.active.lock().unwrap().clone();
                    if wanted(&event, active.as_ref()) {
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
                        match core.hub.snapshot(&active).await {
                            Ok(snapshot) => core.push_snapshot(snapshot),
                            Err(err) => tracing::debug!(%err, "resync read failed"),
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
        assert!(wanted(&upsert("open"), Some(&id("open"))));
        assert!(!wanted(&upsert("other"), Some(&id("open"))));
        assert!(!wanted(&upsert("other"), None));
        let status = AgentDomainEvent::StatusChanged {
            asid: id("other"),
            status: AgentSessionStatus::Busy,
            error: None,
            seq: 1,
        };
        assert!(wanted(&status, Some(&id("open"))), "a row shows status");
        let removed = |asid: &str| AgentDomainEvent::TimelineRemoved {
            asid: id(asid),
            ids: vec!["x".into()],
            seq: 2,
        };
        assert!(wanted(&removed("open"), Some(&id("open"))));
        assert!(!wanted(&removed("other"), Some(&id("open"))));
        let resolved = AgentDomainEvent::FormResolved {
            asid: id("open"),
            form_id: "f".into(),
            reason: None,
            seq: 3,
        };
        assert!(
            wanted(&resolved, Some(&id("open"))),
            "the open session's forms"
        );
    }
}
