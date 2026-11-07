//! Muqun Desktop UI: rune-kit chrome and controls over the core's [`Update`]s.
//!
//! `UiState` is the main-thread model. [`apply_updates`](UiState::apply_updates) folds
//! every core update into it, each page sends [`Command`]s back, and [`view`] lays the
//! shell out from it.

mod composer;
mod home;
mod inbox;
mod inspector;
pub mod scenes;
mod sessions;
mod settings;
mod shell;
mod tasks;
mod timeline;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use muqun_gateway::agents::directories::DirectoryEntry;
use muqun_gateway::agents::{
    AgentDomainEvent, AgentPart, AgentProject, AgentSessionId, AgentSessionInfo,
    AgentSessionStatus, FormRequest, PermissionRequest, TimelineItem,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use rune::{Cx, El, KeyEvent, TextEdit, Tx};
use rune_kit::diff::{parse, DiffState};
use rune_kit::shell::{is_sidebar_toggle, PaneState};
use rune_kit::{Kit, ToastRegion};

use crate::config::{ConfigEdit, DesktopConfig, Scheme};
use crate::hub::CatalogView;
use crate::hub::{SavedPermission, ShellRow, TaskRow, WorktreeRow};
use crate::model::commands::{self, Command, CommandSender};
use crate::model::compose::{BranchInfo, ComposerUpdate, FileHit, InboxItem, StagedFile};
use crate::model::inbox::{Inbox, UiAction, UiError, Update};
use crate::model::projects::DirInfo;
use crate::model::types::{DiffStat, SESSION_LIST_LIMIT};

pub use settings::SettingsTab;

/// What the main pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Home,
    Session,
    Inbox,
    Tasks,
    Settings,
}

/// The inspector's tabs (worktrees live under Git).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorTab {
    Queue,
    Git,
    Subagents,
    Saved,
    Shells,
}

/// Which composer a submit came from, so the right draft clears on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Session,
    Start,
}

/// A transient notice (theme service answers, failed sends).
#[derive(Debug, Clone)]
pub struct ToastItem {
    pub id: u64,
    pub title: String,
    pub detail: Option<String>,
}

const MAX_TOASTS: usize = 3;

static INITIAL_CONFIG: OnceLock<DesktopConfig> = OnceLock::new();

/// Hand the loaded config to the UI before the window opens.
pub fn seed_config(config: DesktopConfig) {
    let _ = INITIAL_CONFIG.set(config);
}

/// Main-thread UI state owned by Rune.
pub struct UiState {
    pub commands: CommandSender,
    /// A mirror of the owner's config: what the pages show, kept in step with
    /// every [`ConfigEdit`] sent to the core.
    pub cfg: DesktopConfig,
    pub dark: bool,
    pub mode: Mode,
    pub settings_tab: SettingsTab,
    pub sidebar: PaneState,
    pub right: PaneState,
    pub right_open: bool,
    pub inspector_tab: InspectorTab,

    pub sessions: Vec<AgentSessionInfo>,
    pub directories: HashMap<String, DirInfo>,
    pub known_projects: HashMap<String, Vec<AgentProject>>,
    /// The start page's project choices, per agent.
    pub agent_projects: HashMap<String, Vec<AgentProject>>,
    pub agents: Vec<AgentDiscoveryInfo>,

    pub selected: Option<String>,
    pub timeline: Vec<TimelineItem>,
    /// Diffs drawn inline in the timeline, by item id, with the patch they came from.
    pub inline_diffs: HashMap<String, (String, DiffState)>,
    pub earlier_exhausted: bool,
    pub status: String,
    pub hub_label: String,
    pub error: Option<UiError>,

    pub draft: TextEdit,
    pub start_draft: TextEdit,
    pub start_agent: String,
    pub start_cwd: TextEdit,
    pub start_worktree: bool,
    pub completions: Vec<DirectoryEntry>,
    pub staged: Vec<StagedFile>,
    pub queue: Vec<InboxItem>,
    pub file_hits: Vec<FileHit>,
    pub catalog: Option<Arc<CatalogView>>,
    pub branches: HashMap<String, BranchInfo>,
    pub auto_approve: HashMap<String, bool>,
    pub next_token: u64,
    pub pending_submit: Option<(u64, Slot)>,

    pub diff_stat: DiffStat,
    pub diff: Option<DiffState>,
    pub show_diff: bool,
    pub revert_staged: bool,

    pub tasks: Vec<TaskRow>,
    pub task_cwd: Option<String>,
    pub task_goal: TextEdit,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
    pub shells: Vec<ShellRow>,
    pub worktrees: Vec<WorktreeRow>,

    pub search: TextEdit,
    pub open_projects: HashMap<String, bool>,
    pub full_projects: HashSet<String>,
    /// Collapsibles the owner opened, by item id.
    pub open_items: HashSet<String>,

    /// Pending prompts per session, from snapshots and events.
    pub waiting_perms: HashMap<String, Vec<PermissionRequest>>,
    pub waiting_forms: HashMap<String, Vec<FormRequest>>,
    /// Form answers, by `form id / field key`.
    pub form_inputs: HashMap<String, TextEdit>,

    pub agent_inputs: HashMap<String, TextEdit>,
    pub agent_errors: HashMap<String, String>,
    pub cwd_input: TextEdit,
    pub toasts: Vec<ToastItem>,
    toast_seq: u64,
    /// A screenshot scene is showing: live core updates are ignored.
    pub demo: bool,
}

impl UiState {
    pub fn new(commands: CommandSender) -> Self {
        let cfg = INITIAL_CONFIG.get().cloned().unwrap_or_default();
        let dark = !matches!(cfg.appearance.scheme, Scheme::Light);
        let mut draft = TextEdit::new("");
        draft.focused = true;
        let cwd_input = TextEdit::new(
            cfg.general
                .default_cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        Self {
            commands,
            dark,
            mode: Mode::Home,
            settings_tab: SettingsTab::Agents,
            sidebar: PaneState::new(260.0).range(180.0, 420.0),
            right: PaneState::new(320.0).range(240.0, 520.0),
            right_open: false,
            inspector_tab: InspectorTab::Queue,
            sessions: Vec::new(),
            directories: HashMap::new(),
            known_projects: HashMap::new(),
            agent_projects: HashMap::new(),
            agents: Vec::new(),
            selected: None,
            timeline: Vec::new(),
            inline_diffs: HashMap::new(),
            earlier_exhausted: false,
            status: "Starting…".into(),
            hub_label: String::new(),
            error: None,
            draft,
            start_draft: TextEdit::new(""),
            start_agent: cfg.general.last_agent.clone().unwrap_or_default(),
            start_cwd: TextEdit::new(""),
            start_worktree: false,
            completions: Vec::new(),
            staged: Vec::new(),
            queue: Vec::new(),
            file_hits: Vec::new(),
            catalog: None,
            branches: HashMap::new(),
            auto_approve: HashMap::new(),
            next_token: 1,
            pending_submit: None,
            diff_stat: DiffStat::default(),
            diff: None,
            show_diff: false,
            revert_staged: false,
            tasks: Vec::new(),
            task_cwd: None,
            task_goal: TextEdit::new(""),
            descendants: Vec::new(),
            saved_permissions: Vec::new(),
            shells: Vec::new(),
            worktrees: Vec::new(),
            search: TextEdit::new(""),
            open_projects: HashMap::new(),
            full_projects: HashSet::new(),
            open_items: HashSet::new(),
            waiting_perms: HashMap::new(),
            waiting_forms: HashMap::new(),
            form_inputs: HashMap::new(),
            agent_inputs: HashMap::new(),
            agent_errors: HashMap::new(),
            cwd_input,
            toasts: Vec::new(),
            toast_seq: 0,
            demo: false,
            cfg,
        }
    }

    // ---- helpers the pages share -------------------------------------------------

    pub fn send(&self, command: Command) {
        commands::send(&self.commands, command);
    }

    /// Fold a config edit into the mirror and tell the core.
    pub fn edit(&mut self, edit: ConfigEdit) {
        self.cfg.apply(edit.clone());
        self.send(Command::Edit(edit));
    }

    pub fn session_by_id(&self, asid: &str) -> Option<&AgentSessionInfo> {
        self.sessions.iter().find(|s| s.asid.0 == asid)
    }

    pub fn current(&self) -> Option<&AgentSessionInfo> {
        self.selected.as_deref().and_then(|a| self.session_by_id(a))
    }

    pub fn busy(&self) -> bool {
        self.current().is_some_and(|s| {
            matches!(
                s.status,
                AgentSessionStatus::Busy | AgentSessionStatus::Retry
            )
        })
    }

    pub fn agent_name(&self, id: &str) -> String {
        self.agents
            .iter()
            .find(|a| a.id == id)
            .map_or_else(|| id.to_string(), |a| a.name.clone())
    }

    pub fn toast(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.toast_seq += 1;
        self.toasts.push(ToastItem {
            id: self.toast_seq,
            title: title.into(),
            detail,
        });
        while self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }

    pub fn token(&mut self, slot: Slot) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        self.pending_submit = Some((token, slot));
        token
    }

    pub fn is_dark(&self, system: bool) -> bool {
        match self.cfg.appearance.scheme {
            Scheme::System => system,
            Scheme::Dark => true,
            Scheme::Light => false,
        }
    }

    pub fn set_scheme(&mut self, scheme: Scheme) {
        self.dark = !matches!(scheme, Scheme::Light);
        self.edit(ConfigEdit::Scheme(scheme));
    }

    /// Waiting prompts across every session: (asid, permission or form count).
    pub fn waiting_count(&self) -> usize {
        let perms: usize = self.waiting_perms.values().map(Vec::len).sum();
        let forms: usize = self.waiting_forms.values().map(Vec::len).sum();
        let tasks = self.tasks.iter().filter(|t| t.state == "waiting").count();
        perms + forms + tasks
    }

    pub fn open_session(&mut self, asid: &str) {
        let changed = self.selected.as_deref() != Some(asid);
        self.selected = Some(asid.to_string());
        self.mode = Mode::Session;
        self.show_diff = false;
        if changed {
            self.timeline.clear();
            self.inline_diffs.clear();
            self.queue.clear();
            self.staged.clear();
            self.diff_stat = DiffStat::default();
            self.diff = None;
            self.earlier_exhausted = false;
        }
        self.status = format!("Opening {asid}");
        self.send(Command::OpenSession(AgentSessionId(asid.to_string())));
    }

    pub fn go(&mut self, mode: Mode) {
        self.mode = mode;
        match mode {
            Mode::Tasks => self.send(Command::RefreshTasks),
            Mode::Home => self.send(Command::LoadProjects {
                agent: self.start_agent.clone(),
            }),
            Mode::Settings => self.send(Command::ThemeRefresh),
            _ => {}
        }
    }

    /// Ask the core for what the inspector's current tab shows.
    pub fn refresh_inspector(&mut self) {
        let Some(info) = self.current().cloned() else {
            return;
        };
        let asid = info.asid.clone();
        match self.inspector_tab {
            InspectorTab::Queue => {}
            InspectorTab::Git => {
                if let Some(directory) = info.directory.clone() {
                    self.send(Command::LoadWorktrees {
                        agent: info.agent_id.clone(),
                        directory,
                    });
                }
            }
            InspectorTab::Subagents => self.send(Command::LoadDescendants { asid }),
            InspectorTab::Saved => self.send(Command::LoadSavedPermissions { asid }),
            InspectorTab::Shells => self.send(Command::LoadShells {
                agent: info.agent_id.clone(),
                directory: info.directory.clone(),
            }),
        }
    }

    pub fn toggle_inspector(&mut self) {
        self.right_open = !self.right_open;
        self.right.set_open(self.right_open);
        if self.right_open {
            self.refresh_inspector();
        }
    }

    /// The composer's catalogue, branch and bypass state for a freshly opened session.
    fn session_opened(&mut self, info: &AgentSessionInfo) {
        use crate::model::compose::ComposeCommand as C;
        self.send(Command::Compose(C::Catalog {
            agent: info.agent_id.clone(),
            directory: info.directory.clone(),
            asid: Some(info.asid.clone()),
        }));
        self.send(Command::Compose(C::AutoApprove {
            asid: info.asid.clone(),
        }));
        if let Some(cwd) = info.directory.clone() {
            self.send(Command::Compose(C::Branch { cwd }));
        }
        if self.right_open {
            self.refresh_inspector();
        }
    }

    // ---- core → UI ---------------------------------------------------------------

    pub fn apply_updates(&mut self, updates: Vec<Update>) {
        if self.demo {
            return;
        }
        for update in updates {
            match update {
                Update::Agents(agents) => self.on_agents(agents),
                Update::AgentSessions { agent, sessions } => {
                    self.sessions.retain(|s| s.agent_id != agent);
                    self.merge_sessions(sessions);
                    if self.error.as_ref().is_some_and(|e| e.unreachable) {
                        self.error = None;
                    }
                }
                Update::SessionCreated(info) => self.merge_sessions(vec![*info]),
                Update::SessionGone(asid) => {
                    self.sessions.retain(|s| s.asid.0 != asid);
                    self.waiting_perms.remove(&asid);
                    self.waiting_forms.remove(&asid);
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.selected = None;
                        self.timeline.clear();
                        self.inline_diffs.clear();
                        self.mode = Mode::Home;
                    }
                }
                Update::Snapshot(snapshot) => self.on_snapshot(*snapshot),
                Update::Event(event) => self.apply_event(*event),
                Update::Error(err) => {
                    self.status = err.message.clone();
                    self.error = Some(err);
                }
                Update::Projects { agent, projects } => {
                    self.agent_projects.insert(agent, projects);
                }
                Update::Completions { prefix, entries } => {
                    if prefix == self.start_cwd.text {
                        self.completions = entries;
                    }
                }
                Update::Ui(action) => self.on_action(action),
                Update::Earlier {
                    asid,
                    items,
                    exhausted,
                } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        let mut older = Vec::new();
                        for item in items {
                            self.ingest(&item);
                            older.push(item);
                        }
                        older.append(&mut self.timeline);
                        self.timeline = older;
                        self.earlier_exhausted = exhausted;
                    }
                }
                Update::Composer(update) => self.on_composer(*update),
                Update::Theme(update) => {
                    let crate::theme::service::ThemeUpdate::Notice(notice) = *update;
                    self.status = notice.message.clone();
                    self.toast(notice.message, notice.detail);
                }
                Update::Directories(found) => self.directories.extend(found),
                Update::KnownProjects { agent, projects } => {
                    self.known_projects.insert(agent, projects);
                }
                Update::Diff(stat) => {
                    if self.selected.as_deref() == Some(stat.asid.as_str()) {
                        let patch: String = stat
                            .paths
                            .iter()
                            .map(|f| f.patch.as_str())
                            .filter(|p| !p.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.diff = (!patch.is_empty()).then(|| DiffState::new(parse(&patch)));
                        self.diff_stat = stat;
                    }
                }
                Update::Tasks { rows, default_cwd } => {
                    self.tasks = rows;
                    self.task_cwd = default_cwd;
                }
                Update::Descendants { asid, sessions } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.descendants = sessions;
                    }
                }
                Update::SavedPermissions { asid, items } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.saved_permissions = items;
                    }
                }
                Update::Shells(rows) => self.shells = rows,
                Update::Worktrees(rows) => self.worktrees = rows,
                Update::HubMode(mode) => self.hub_label = mode.to_string(),
            }
        }
    }

    fn on_agents(&mut self, agents: Vec<AgentDiscoveryInfo>) {
        self.status = format!("{} agent(s) discovered", agents.len());
        let usable = |a: &AgentDiscoveryInfo| a.enabled;
        let known = agents.iter().any(|a| a.id == self.start_agent);
        if !known {
            self.start_agent = agents
                .iter()
                .find(|a| usable(a))
                .or(agents.first())
                .map(|a| a.id.clone())
                .unwrap_or_default();
        }
        let first = self.agents.is_empty();
        self.agents = agents;
        if first && self.start_cwd.text.is_empty() {
            let cwd = self.cfg.general.initial_cwd(&self.start_agent);
            self.start_cwd = TextEdit::new(cwd.display().to_string());
            if !self.start_agent.is_empty() {
                self.send(Command::LoadProjects {
                    agent: self.start_agent.clone(),
                });
            }
        }
    }

    fn merge_sessions(&mut self, incoming: Vec<AgentSessionInfo>) {
        for info in incoming {
            match self.sessions.iter_mut().find(|s| s.asid.0 == info.asid.0) {
                Some(slot) => *slot = info,
                None => self.sessions.push(info),
            }
        }
        self.sessions
            .sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        self.sessions.truncate(SESSION_LIST_LIMIT);
    }

    fn on_snapshot(&mut self, snapshot: muqun_gateway::agents::ports::AgentSessionSnapshot) {
        let asid = snapshot.info.asid.0.clone();
        let fresh = self.selected.as_deref() != Some(asid.as_str());
        if fresh {
            self.inline_diffs.clear();
        }
        self.selected = Some(asid.clone());
        if self.mode == Mode::Home {
            self.mode = Mode::Session;
        }
        self.merge_sessions(vec![snapshot.info.clone()]);
        self.timeline.clear();
        for item in &snapshot.timeline {
            self.ingest(item);
        }
        self.timeline = snapshot.timeline;
        self.waiting_perms
            .insert(asid.clone(), snapshot.permissions);
        self.waiting_forms.insert(asid.clone(), snapshot.forms);
        self.queue = InboxItem::list(&snapshot.inbox);
        self.status = format!("{} messages", self.timeline.len());
        if self.error.as_ref().is_some_and(|e| e.unreachable) {
            self.error = None;
        }
        if fresh {
            self.earlier_exhausted = false;
            self.session_opened(&snapshot.info);
        }
    }

    fn apply_event(&mut self, event: AgentDomainEvent) {
        let current =
            |s: &Self, asid: &AgentSessionId| s.selected.as_deref() == Some(asid.0.as_str());
        match event {
            AgentDomainEvent::TimelineUpsert { asid, items, .. } => {
                if !current(self, &asid) {
                    return;
                }
                for item in items {
                    self.ingest(&item);
                    match self.timeline.iter_mut().find(|t| t.id == item.id) {
                        Some(slot) => *slot = item,
                        None => self.timeline.push(item),
                    }
                }
            }
            AgentDomainEvent::TimelineRemoved { asid, ids, .. } => {
                if current(self, &asid) {
                    self.timeline.retain(|t| !ids.contains(&t.id));
                    for id in &ids {
                        self.inline_diffs.remove(id);
                    }
                }
            }
            AgentDomainEvent::SessionUpdated { info, .. } => self.merge_sessions(vec![*info]),
            AgentDomainEvent::StatusChanged {
                asid,
                status,
                error,
                ..
            } => {
                if let Some(slot) = self.sessions.iter_mut().find(|s| s.asid == asid) {
                    slot.status = status;
                }
                if let Some(err) = error {
                    self.error = Some(UiError {
                        message: err.message,
                        detail: Some(err.name),
                        unreachable: false,
                    });
                }
            }
            AgentDomainEvent::PermissionPending { asid, request, .. } => {
                let list = self.waiting_perms.entry(asid.0).or_default();
                if !list.iter().any(|p| p.id == request.id) {
                    list.push(request);
                }
            }
            AgentDomainEvent::PermissionResolved {
                asid, request_id, ..
            } => {
                if let Some(list) = self.waiting_perms.get_mut(&asid.0) {
                    list.retain(|p| p.id != request_id);
                }
            }
            AgentDomainEvent::FormPending { asid, request, .. } => {
                let list = self.waiting_forms.entry(asid.0).or_default();
                if !list.iter().any(|f| f.id == request.id) {
                    list.push(request);
                }
            }
            AgentDomainEvent::FormResolved { asid, form_id, .. } => {
                if let Some(list) = self.waiting_forms.get_mut(&asid.0) {
                    list.retain(|f| f.id != form_id);
                }
                self.form_inputs
                    .retain(|k, _| !k.starts_with(&format!("{form_id}/")));
            }
            AgentDomainEvent::CompactionChanged { asid, status, .. } => {
                if current(self, &asid) {
                    self.status = format!("Compaction {status:?}").to_lowercase();
                }
            }
            AgentDomainEvent::RevertChanged { asid, revert, .. } => {
                if current(self, &asid) {
                    self.revert_staged = revert.is_some();
                }
            }
            AgentDomainEvent::InboxChanged { asid, items, .. } => {
                if current(self, &asid) {
                    self.queue = InboxItem::list(&items);
                }
            }
            AgentDomainEvent::WorktreeChanged { .. } => {
                if self.right_open && self.inspector_tab == InspectorTab::Git {
                    self.refresh_inspector();
                }
            }
            AgentDomainEvent::Resync { asid, .. } => {
                let ours = asid.0.is_empty() || current(self, &asid);
                if let (true, Some(open)) = (ours, self.selected.clone()) {
                    self.send(Command::OpenSession(AgentSessionId(open)));
                }
            }
        }
    }

    /// Parse the item's diff (a diff part or an edit tool's files) for inline drawing.
    fn ingest(&mut self, item: &TimelineItem) {
        let patch = match &item.part {
            AgentPart::Diff { diff, .. } => diff.clone(),
            AgentPart::Tool(call) => timeline::tool_patch(call),
            _ => return,
        };
        if patch.trim().is_empty() {
            self.inline_diffs.remove(&item.id);
        } else if self
            .inline_diffs
            .get(&item.id)
            .is_none_or(|(old, _)| *old != patch)
        {
            let state = DiffState::new(parse(&patch));
            self.inline_diffs.insert(item.id.clone(), (patch, state));
        }
    }

    fn on_action(&mut self, action: UiAction) {
        match action {
            UiAction::Open(asid) => self.open_session(&asid),
            UiAction::ToggleDiffPanel => self.show_diff = !self.show_diff,
            UiAction::OpenDiffFile { asid, path } => {
                if self.selected.as_deref() == Some(asid.as_str()) {
                    self.show_diff = true;
                    self.status = path;
                }
            }
            UiAction::ProjectPicked(path) => {
                self.start_cwd = TextEdit::new(path.display().to_string());
            }
            UiAction::FoldersDropped(paths) => {
                let mut text = self.draft.text.clone();
                for path in paths {
                    if !text.is_empty() && !text.ends_with(' ') {
                        text.push(' ');
                    }
                    text.push_str(&path.display().to_string());
                }
                self.draft = TextEdit::new(text);
            }
            UiAction::ImagePasted(png) => composer::stage_png(self, png),
            UiAction::FilesPicked(paths) => {
                for path in paths {
                    composer::stage_path(self, path);
                }
            }
            UiAction::SubmitComposer => composer::submit_session(self),
            UiAction::ComposerEdited
            | UiAction::ComposerKey { .. }
            | UiAction::QaStep(_)
            | UiAction::DropHover(_) => {}
        }
    }

    fn on_composer(&mut self, update: ComposerUpdate) {
        match update {
            ComposerUpdate::Files { hits } => self.file_hits = hits,
            ComposerUpdate::Catalog { view, .. } => self.catalog = Some(view),
            ComposerUpdate::Staged { result, .. } => match result {
                Ok(file) => self.staged.push(file),
                Err(err) => self.toast("Attachment failed", Some(err)),
            },
            ComposerUpdate::Submitted { token, ok } => {
                let Some((pending, slot)) = self.pending_submit else {
                    return;
                };
                if pending != token {
                    return;
                }
                self.pending_submit = None;
                if ok {
                    match slot {
                        Slot::Session => {
                            self.draft = TextEdit::new("");
                            self.draft.focused = true;
                        }
                        Slot::Start => self.start_draft = TextEdit::new(""),
                    }
                    self.staged.clear();
                    self.status = "Sent".into();
                } else {
                    self.status = "Send failed".into();
                }
            }
            ComposerUpdate::Inbox { asid, items } => {
                if self.selected.as_deref() == Some(asid.as_str()) {
                    self.queue = items;
                }
            }
            ComposerUpdate::Branch { cwd, info } => {
                self.branches.insert(cwd, info);
            }
            ComposerUpdate::Peers { .. } => {}
            ComposerUpdate::AutoApprove { asid, enabled } => {
                if let Some(on) = enabled {
                    self.auto_approve.insert(asid, on);
                }
            }
        }
    }
}

// ---- shared view helpers ------------------------------------------------------------

pub fn muted(k: &Kit, s: impl Into<String>) -> El {
    rune::text(s).font(k.ty.small).color(k.c.text_muted).wrap()
}

pub fn heading(k: &Kit, s: impl Into<String>) -> El {
    rune::text(s).font(k.ty.ui_medium).color(k.c.text_base)
}

pub fn title_of(info: &AgentSessionInfo) -> String {
    if info.title.trim().is_empty() {
        info.asid.0.clone()
    } else {
        info.title.clone()
    }
}

pub fn on_key(s: &mut UiState, ev: &KeyEvent, _tx: &mut Tx) -> bool {
    if is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        true
    } else if ev.is_command(',') && !ev.mods.shift && !ev.mods.alt {
        s.go(Mode::Settings);
        true
    } else {
        false
    }
}

pub fn view(s: &UiState, cx: &mut Cx<UiState>) -> El {
    let k = Kit::neutral(s.is_dark(cx.system_dark()));
    let mut layer: Vec<El> = Vec::new();
    if !s.toasts.is_empty() {
        let mut region = ToastRegion::new();
        for t in &s.toasts {
            let id = t.id;
            let mut toast = rune_kit::Toast::new(format!("toast-{id}"), t.title.clone())
                .on_close(move |s: &mut UiState| s.toasts.retain(|t| t.id != id));
            if let Some(d) = &t.detail {
                toast = toast.description(d.clone());
            }
            region = region.toast(toast.build(&k));
        }
        layer.push(region.build(&k));
    }
    let body = shell::view(s, cx, &k);
    rune_kit::root(body, layer)
}

/// Poll the inbox on a background thread and push batches into Rune state.
pub fn spawn_inbox_pump(inbox: Arc<Inbox>, handle: rune::AppHandle<UiState>) {
    inbox.set_wake(Arc::new({
        let handle = handle.clone();
        move || handle.update(|_, _| {})
    }));
    std::thread::Builder::new()
        .name("muqun-ui-inbox".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(16));
            let batch = inbox.take();
            if batch.is_empty() {
                continue;
            }
            handle.update(move |s, _| s.apply_updates(batch));
        })
        .expect("inbox pump");
}
