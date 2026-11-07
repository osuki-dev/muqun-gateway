//! Muqun Desktop UI: rune-kit chrome and controls over the core's [`Update`]s.
//!
//! `UiState` is the main-thread model. [`apply_updates`](UiState::apply_updates) folds
//! every core update into it, each page sends [`Command`]s back, and [`view`] lays the
//! shell out from it.

mod agent_marks;
mod agents;
mod art;
mod composer;
pub mod copy;
pub mod fx;
mod home;
mod icons;
mod inbox;
mod inspector;
mod labels;
mod palette;
mod parts;
pub mod scenes;
mod sessions;
mod settings;
mod shell;
mod splash;
mod tasks;
mod terminals;
mod tiles;
mod timeline;
mod tray;
mod viewer;
mod voice;
mod welcome;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use muqun_gateway::agents::directories::DirectoryEntry;
use muqun_gateway::agents::{
    AgentDomainEvent, AgentProject, AgentSessionId, AgentSessionInfo, AgentSessionStatus,
    FormRequest, ModelRef, PermissionRequest,
};
use muqun_gateway::platform::discovery::AgentDiscoveryInfo;
use rune::{Cx, El, ImageCache, KeyEvent, TextEdit, Tx};
use rune_kit::diff::{parse, DiffState};
use rune_kit::shell::{is_sidebar_toggle, PaneState};
use rune_kit::{Kit, ScrollBar, ToastRegion};

use crate::config::{ConfigEdit, DesktopConfig, Scheme};
use crate::hub::CatalogView;
use crate::hub::{PeerMention, SavedPermission, ShellRow, TaskRow, WorktreeRow};
use crate::model::commands::{self, Command, CommandSender};
use crate::model::compose::{BranchInfo, ComposerUpdate, FileHit, InboxItem, StagedFile};
use crate::model::inbox::{Inbox, UiAction, UiError, Update};
use crate::model::projects::DirInfo;
use crate::model::types::{DiffStat, SESSIONS_KEPT};
use crate::theme::color_scheme::ColorScheme;
use crate::theme::look::Look;
use crate::theme::service::{GalleryView, Job, ThemeUpdate};
use crate::theme::store::ThemeEntry;

pub use settings::SettingsTab;
pub use tray::words as tray_words;

/// What the main pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Home,
    Session,
    Inbox,
    Tasks,
    Terminals,
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

/// How many gallery themes the Themes tab lists before "Show more".
const GALLERY_PAGE: usize = 8;

/// A pane's grip or toggle was touched: the width may have changed and
/// [`UiState::persist_layout`] should look (set from the pane access
/// functions, read by the inbox pump).
static LAYOUT_DIRTY: AtomicBool = AtomicBool::new(false);

fn sidebar_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.sidebar
}

fn right_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.right
}

/// The one open dropdown (or row menu), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenMenu {
    /// The session composer's model, effort, branch menus.
    Model,
    Effort,
    Branch,
    /// The start page's model, mode and branch menus.
    StartModel,
    StartMode,
    StartBranch,
    /// A session row's actions (by asid).
    Session(String),
    /// A project row's actions (by project id / root).
    Project(String),
    /// A queued prompt chip above the composer (by inbox item id).
    Queued(String),
    /// The session header's overflow (rename, delete …).
    SessionMore,
    /// A split tile's menu (by tile index).
    Tile(usize),
}

/// The view state of one open session: what the timeline and composer read.
/// The focused session's slot lives in [`UiState`]'s own fields (so the
/// composer keeps reading `s.draft` …); every other
/// open tile keeps its own here and swaps in when it takes focus.
#[derive(Default)]
///
/// Timelines are not in the slot: every open session's lives in
/// `UiState::timelines` by asid, focused or parked.
pub struct SessionSlot {
    pub draft: TextEdit,
    pub queue: Vec<InboxItem>,
    pub staged: Vec<StagedFile>,
    pub diff_stat: DiffStat,
    pub diff: Option<DiffState>,
    pub show_diff: bool,
    pub revert_staged: bool,
    pub catalog: Option<Arc<CatalogView>>,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
}

/// One pane of the split session view. `asid` is `None` while the tile
/// waits for a session to be picked into it.
pub struct Tile {
    pub asid: Option<String>,
    pub slot: SessionSlot,
}

/// How many sessions may be open side by side.
pub const MAX_TILES: usize = 4;

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
    /// The open dropdown or row menu.
    pub menu: Option<OpenMenu>,

    pub sessions: Vec<AgentSessionInfo>,
    pub directories: HashMap<String, DirInfo>,
    pub known_projects: HashMap<String, Vec<AgentProject>>,
    /// The start page's project choices, per agent.
    pub agent_projects: HashMap<String, Vec<AgentProject>>,
    pub agents: Vec<AgentDiscoveryInfo>,

    pub selected: Option<String>,
    /// Timelines on screen, by asid: the focused session's and every tile's
    /// (`timeline::SessionTimeline`; drawn by `timeline::session_view`).
    pub timelines: HashMap<String, timeline::SessionTimeline>,
    /// A timeline picture opened full size (`timeline::overlay`).
    pub zoom: Option<PathBuf>,
    /// A subagent's session shown read-only over its parent (`timeline::overlay`).
    pub detail: Option<timeline::DetailSheet>,
    /// The session whose context sheet is open (`timeline::context_meter`).
    pub context_open: Option<String>,
    /// Account rate windows by agent, once asked for (`None`: it has none).
    pub account_limits: HashMap<String, Option<serde_json::Value>>,
    pub status: String,
    pub hub_label: String,
    pub error: Option<UiError>,

    pub draft: TextEdit,
    pub start_draft: TextEdit,
    pub start_agent: String,
    pub start_cwd: TextEdit,
    pub start_worktree: bool,
    /// The start page's model and mode picks; `None` leaves the agent's own.
    pub start_model: Option<ModelRef>,
    pub start_mode: Option<String>,
    pub start_catalog: Option<Arc<CatalogView>>,
    pub completions: Vec<DirectoryEntry>,
    pub staged: Vec<StagedFile>,
    pub queue: Vec<InboxItem>,
    pub file_hits: Vec<FileHit>,
    pub catalog: Option<Arc<CatalogView>>,
    pub branches: HashMap<String, BranchInfo>,
    /// Computers `@` may name.
    pub peers: Vec<PeerMention>,
    /// Esc dismissed a composer popup at this query start; forgotten when the
    /// trigger leaves the draft (App `dismissedAt`).
    pub popup_dismissed_at: Option<usize>,
    /// File paths inserted as `@` mentions (App `fileTokens`), for highlight /
    /// atomic delete of typed paths still standing in the draft.
    pub mention_file_tokens: Vec<String>,
    /// Previous draft text + caret, for App-style atomic mention backspace.
    pub draft_snap: String,
    pub draft_snap_caret: usize,
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
    /// Inline rename of a session row (its asid) and the text being typed.
    pub rename_asid: Option<String>,
    pub rename_edit: TextEdit,
    /// Inline rename of a project row (its id) and the text being typed.
    pub rename_project: Option<String>,
    pub project_edit: TextEdit,
    pub open_projects: HashMap<String, bool>,
    pub full_projects: HashSet<String>,

    /// Pending prompts per session, from snapshots and events.
    pub waiting_perms: HashMap<String, Vec<PermissionRequest>>,
    pub waiting_forms: HashMap<String, Vec<FormRequest>>,
    /// Form answers, by `form id / field key`.
    pub form_inputs: HashMap<String, TextEdit>,

    pub agent_inputs: HashMap<String, TextEdit>,
    pub agent_errors: HashMap<String, String>,
    pub cwd_input: TextEdit,
    /// Themes: the library, the gallery, running installs, the link field.
    pub theme_library: Arc<Vec<ThemeEntry>>,
    pub theme_gallery: Arc<GalleryView>,
    /// Gallery card pictures keyed by theme id (`ThemeUpdate::Preview`).
    pub theme_previews: HashMap<String, PathBuf>,
    pub theme_jobs: HashMap<String, Job>,
    pub theme_url: TextEdit,
    pub gallery_limit: usize,
    pub toasts: Vec<ToastItem>,
    toast_seq: u64,
    /// Theme / cover decode cache (wakes the app when an image is ready).
    pub images: Option<ImageCache>,
    /// Launch / theme-switch splash while the core warms up.
    pub splash: Option<splash::Splash>,
    /// First agents discovery finished (empty list still counts).
    pub agents_seen: bool,
    /// A screenshot scene is showing: live core updates are ignored.
    pub demo: bool,
    /// What the desktop keeps on disk (Settings → Storage), once scanned.
    pub storage: Option<crate::model::storage::StorageReport>,
    /// The Terminals page (`ui/terminals.rs`).
    pub terms: terminals::TermState,
    /// What the status icon last showed (`ui/tray.rs`).
    tray_shown: Option<(crate::config::Language, Vec<(String, String)>)>,
    /// The command palette (`ui/palette.rs`), when open.
    pub palette: Option<palette::Palette>,
    /// Dictation in progress (`ui/voice.rs`).
    pub voice: Option<voice::Dictation>,
    /// Settings → Voice's fields.
    pub voice_form: voice::VoiceForm,
    /// The file viewer overlay ([`UiState::open_file`]).
    pub viewer: Option<viewer::Viewer>,
    /// Sessions open side by side (empty when one session fills the pane).
    pub tiles: Vec<Tile>,
    /// The tile the keyboard and the composer belong to.
    pub focus: usize,
    /// Where the first column ends, as a fraction of the split's width.
    pub tile_split: f32,
    /// An empty tile waiting for the next opened session.
    pub pending_tile: Option<usize>,
    /// The split's width on screen, for the grip's drag.
    pub tile_grid_w: f32,
    /// The saved split was put back once sessions were listed.
    tiles_restored: bool,
    /// Agents whose session list has arrived (split restore waits for them).
    listed_agents: HashSet<String>,
}

impl UiState {
    pub fn new(commands: CommandSender) -> Self {
        let cfg = INITIAL_CONFIG.get().cloned().unwrap_or_default();
        let dark = !matches!(cfg.appearance.scheme, Scheme::Light);
        let mut draft = TextEdit::new("");
        draft.focused = true;
        let start_draft = TextEdit::new(
            cfg.general
                .drafts
                .get(crate::config::START_DRAFT)
                .cloned()
                .unwrap_or_default(),
        );
        let cwd_input = TextEdit::new(
            cfg.general
                .default_cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        let mut s = Self {
            commands,
            dark,
            mode: Mode::Home,
            settings_tab: SettingsTab::Agents,
            sidebar: PaneState::new(260.0)
                .range(180.0, 420.0)
                .with_width(cfg.ui.sidebar()),
            right: PaneState::new(320.0)
                .range(240.0, 520.0)
                .with_width(cfg.ui.right()),
            right_open: false,
            inspector_tab: InspectorTab::Queue,
            menu: None,
            sessions: Vec::new(),
            directories: HashMap::new(),
            known_projects: HashMap::new(),
            agent_projects: HashMap::new(),
            agents: Vec::new(),
            selected: None,
            timelines: HashMap::new(),
            zoom: None,
            detail: None,
            context_open: None,
            account_limits: HashMap::new(),
            status: "Starting…".into(),
            hub_label: String::new(),
            error: None,
            draft,
            start_draft,
            start_agent: cfg.general.last_agent.clone().unwrap_or_default(),
            start_cwd: TextEdit::new(""),
            start_worktree: false,
            start_model: None,
            start_mode: None,
            start_catalog: None,
            completions: Vec::new(),
            staged: Vec::new(),
            queue: Vec::new(),
            file_hits: Vec::new(),
            catalog: None,
            branches: HashMap::new(),
            peers: Vec::new(),
            popup_dismissed_at: None,
            mention_file_tokens: Vec::new(),
            draft_snap: String::new(),
            draft_snap_caret: 0,
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
            rename_asid: None,
            rename_edit: TextEdit::new(""),
            rename_project: None,
            project_edit: TextEdit::new(""),
            open_projects: HashMap::new(),
            full_projects: HashSet::new(),
            waiting_perms: HashMap::new(),
            waiting_forms: HashMap::new(),
            form_inputs: HashMap::new(),
            agent_inputs: HashMap::new(),
            agent_errors: HashMap::new(),
            cwd_input,
            theme_library: Arc::new(Vec::new()),
            theme_gallery: Arc::new(GalleryView::default()),
            theme_previews: HashMap::new(),
            theme_jobs: HashMap::new(),
            theme_url: TextEdit::new(""),
            gallery_limit: GALLERY_PAGE,
            toasts: Vec::new(),
            toast_seq: 0,
            images: None,
            splash: None,
            agents_seen: false,
            demo: false,
            storage: None,
            viewer: None,
            voice: None,
            palette: None,
            tray_shown: None,
            voice_form: voice::VoiceForm::new(&cfg.voice),
            terms: terminals::TermState::default(),
            tiles: Vec::new(),
            focus: 0,
            tile_split: cfg.ui.tile_split(),
            pending_tile: None,
            tile_grid_w: 0.0,
            listed_agents: HashSet::new(),
            tiles_restored: false,
            cfg,
        };
        s.arm_splash(splash::SplashKind::Boot);
        s.check_voice_key();
        s
    }

    /// Cover the shell with the launch stage; boot also kicks background loads.
    pub fn arm_splash(&mut self, kind: splash::SplashKind) {
        if kind == splash::SplashKind::Boot {
            self.kick_boot_preload();
        }
        if self.cfg.appearance.reduced_motion || self.cfg.appearance.reduced_transparency {
            self.splash = None;
            return;
        }
        self.splash = Some(splash::Splash::start(kind));
    }

    /// Agents, themes, and tasks load while the splash is up.
    fn kick_boot_preload(&mut self) {
        self.send(Command::Refresh);
        self.send(Command::ThemeRefresh);
        self.send(Command::RefreshTasks);
    }

    /// Install the image cache once the window can wake it.
    pub fn install_images(&mut self, waker: rune::Waker) {
        let cache = ImageCache::new(waker);
        let scheme = if self.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        if let Some(bytes) = crate::theme::apply::current_launch(scheme) {
            let _ = cache.load(bytes);
        }
        if let Some(bytes) = crate::theme::apply::current_wallpaper(scheme) {
            let _ = cache.load(bytes);
        }
        if let Some(bytes) = crate::theme::apply::current_artwork(scheme)
            .or_else(|| crate::theme::apply::current_cover(scheme))
        {
            let _ = cache.load(bytes);
        }
        self.images = Some(cache);
    }

    /// Keep the window waking while the splash is animating.
    /// Every frame Rune asks: the splash and a recording's waveform animate.
    pub fn poll_frame(&mut self, t: f64, tx: &mut Tx) -> bool {
        let voice = self.poll_voice();
        self.poll_splash(t, tx) | voice
    }

    pub fn poll_splash(&mut self, _: f64, _: &mut Tx) -> bool {
        let Some(splash) = self.splash.as_mut() else {
            return false;
        };
        let hub_ready = !self.hub_label.is_empty();
        if !splash.tick(self.agents_seen, hub_ready) {
            self.splash = None;
            return false;
        }
        true
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
        agents::name(id, &self.agents)
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
        crate::platform::notify::withdraw(asid);
        if let Some(i) = self.tile_of(asid) {
            if i != self.focus {
                self.focus_tile(i);
                return;
            }
        }
        let changed = self.selected.as_deref() != Some(asid);
        self.selected = Some(asid.to_string());
        self.mode = Mode::Session;
        self.show_diff = false;
        if changed {
            if let Some(tile) = self.tiles.get_mut(self.focus) {
                tile.asid = Some(asid.to_string());
                self.persist_tiles();
                self.sync_watch();
            }
            self.load_draft(asid);
            self.menu = None;
            self.peers.clear();
            self.timelines.remove(asid);
            self.retain_timelines();
            self.queue.clear();
            self.staged.clear();
            self.diff_stat = DiffStat::default();
            self.diff = None;
        }
        self.status = format!("Opening {asid}");
        self.send(Command::OpenSession(AgentSessionId(asid.to_string())));
    }

    pub fn go(&mut self, mode: Mode) {
        self.mode = mode;
        match mode {
            Mode::Tasks => self.send(Command::RefreshTasks),
            Mode::Terminals => self.watch_terminals(),
            Mode::Home => {
                self.send(Command::LoadProjects {
                    agent: self.start_agent.clone(),
                });
                self.refresh_start_catalog();
                self.send(Command::Compose(
                    crate::model::compose::ComposeCommand::ListPeers,
                ));
            }
            Mode::Settings => {
                self.send(Command::ThemeRefresh);
                self.send(Command::ScanStorage);
            }
            _ => {}
        }
    }

    /// Ask for the start page's catalogue (models, modes) and branch list
    /// for the chosen agent and folder. Called when either is picked, not on
    /// every keystroke of the folder field.
    pub fn refresh_start_catalog(&mut self) {
        use crate::model::compose::ComposeCommand as C;
        if self.start_agent.is_empty() {
            return;
        }
        let cwd = self.start_cwd.text.trim().to_string();
        let directory = (!cwd.is_empty()).then(|| cwd.clone());
        self.send(Command::Compose(C::Catalog {
            agent: self.start_agent.clone(),
            directory,
            asid: None,
        }));
        if !cwd.is_empty() {
            self.send(Command::Compose(C::Branch { cwd }));
        }
    }

    /// The draft key of the composer being typed in.
    pub fn draft_key(&self, slot: Slot) -> Option<String> {
        match slot {
            Slot::Start => Some(crate::config::START_DRAFT.to_string()),
            Slot::Session => self.selected.clone(),
        }
    }

    /// Remember the unsent text of a composer (the config store writes it
    /// out after a quiet moment). Does nothing when it has not changed.
    pub fn persist_draft(&mut self, slot: Slot) {
        let Some(key) = self.draft_key(slot) else {
            return;
        };
        let text = match slot {
            Slot::Start => self.start_draft.text.clone(),
            Slot::Session => self.draft.text.clone(),
        };
        let stored = self.cfg.general.drafts.get(&key).map_or("", String::as_str);
        if stored != text {
            self.edit(ConfigEdit::Draft { key, text });
        }
    }

    /// Put the remembered draft of `asid` in the session composer.
    fn load_draft(&mut self, asid: &str) {
        let text = self
            .cfg
            .general
            .drafts
            .get(asid)
            .cloned()
            .unwrap_or_default();
        self.draft = TextEdit::new(text);
        self.draft.focused = true;
    }

    /// Save the panes' widths when they changed (a drag, a double-click
    /// reset). Waits for a drag in progress to end.
    pub fn persist_layout(&mut self) {
        if self.sidebar.is_resizing() || self.right.is_resizing() {
            LAYOUT_DIRTY.store(true, Ordering::Release);
            return;
        }
        let (sidebar, right) = (self.sidebar.width(), self.right.width());
        if (sidebar - self.cfg.ui.sidebar_width).abs() >= 1.0 {
            self.edit(ConfigEdit::SidebarWidth(sidebar));
        }
        if (right - self.cfg.ui.right_width).abs() >= 1.0 {
            self.edit(ConfigEdit::RightWidth(right));
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
        self.send(Command::Compose(C::ListPeers));
        if self.right_open {
            self.refresh_inspector();
        }
    }

    // ---- split tiles ---------------------------------------------------------------

    /// Several sessions share the pane.
    pub fn in_split(&self) -> bool {
        self.tiles.len() >= 2
    }

    pub fn tile_of(&self, asid: &str) -> Option<usize> {
        self.tiles
            .iter()
            .position(|t| t.asid.as_deref() == Some(asid))
    }

    /// Move the focused session's view state out of the top-level fields.
    fn take_slot(&mut self) -> SessionSlot {
        SessionSlot {
            draft: std::mem::replace(&mut self.draft, TextEdit::new("")),
            queue: std::mem::take(&mut self.queue),
            staged: std::mem::take(&mut self.staged),
            diff_stat: std::mem::take(&mut self.diff_stat),
            diff: self.diff.take(),
            show_diff: std::mem::replace(&mut self.show_diff, false),
            revert_staged: std::mem::replace(&mut self.revert_staged, false),
            catalog: self.catalog.take(),
            descendants: std::mem::take(&mut self.descendants),
            saved_permissions: std::mem::take(&mut self.saved_permissions),
        }
    }

    fn put_slot(&mut self, slot: SessionSlot) {
        self.draft = slot.draft;
        self.draft.focused = true;
        self.queue = slot.queue;
        self.staged = slot.staged;
        self.diff_stat = slot.diff_stat;
        self.diff = slot.diff;
        self.show_diff = slot.show_diff;
        self.revert_staged = slot.revert_staged;
        self.catalog = slot.catalog;
        self.descendants = slot.descendants;
        self.saved_permissions = slot.saved_permissions;
    }

    /// Give tile `i` the keyboard: its session becomes the one the top-level
    /// fields describe, the old focused tile keeps its state in its slot.
    pub fn focus_tile(&mut self, i: usize) {
        if i >= self.tiles.len() || i == self.focus {
            self.mode = Mode::Session;
            return;
        }
        let old = self.take_slot();
        self.tiles[self.focus].slot = old;
        let new = std::mem::take(&mut self.tiles[i].slot);
        self.put_slot(new);
        self.focus = i;
        self.selected = self.tiles[i].asid.clone();
        self.menu = None;
        self.mode = Mode::Session;
        self.sync_watch();
        if self.catalog.is_none() {
            if let Some(info) = self.current().cloned() {
                self.session_opened(&info);
            }
        }
    }

    /// Tell the core which sessions besides the focused one are on screen.
    fn sync_watch(&self) {
        let watched: Vec<AgentSessionId> = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.focus)
            .filter_map(|(_, t)| t.asid.clone().map(AgentSessionId))
            .collect();
        self.send(Command::WatchSessions(watched));
    }

    /// Open `asid` beside the current session(s). A session already open in
    /// a tile takes focus instead; a full split replaces the last tile.
    pub fn open_in_tile(&mut self, asid: &str) {
        if let Some(i) = self.tile_of(asid) {
            self.focus_tile(i);
            return;
        }
        self.right_open = false;
        self.right.set_open(false);
        if self.tiles.is_empty() {
            let Some(current) = self.selected.clone() else {
                self.open_session(asid);
                return;
            };
            self.tiles.push(Tile {
                asid: Some(current),
                slot: SessionSlot::default(),
            });
            self.focus = 0;
        }
        let slot = SessionSlot {
            draft: TextEdit::new(
                self.cfg
                    .general
                    .drafts
                    .get(asid)
                    .cloned()
                    .unwrap_or_default(),
            ),
            ..SessionSlot::default()
        };
        if let Some(empty) = self.tiles.iter().position(|t| t.asid.is_none()) {
            self.tiles[empty].asid = Some(asid.to_string());
            self.tiles[empty].slot = slot;
        } else if self.tiles.len() < MAX_TILES {
            self.tiles.push(Tile {
                asid: Some(asid.to_string()),
                slot,
            });
        } else {
            let last = self.tiles.len() - 1;
            let victim = if last == self.focus { last - 1 } else { last };
            self.tiles[victim].asid = Some(asid.to_string());
            self.tiles[victim].slot = slot;
        }
        self.mode = Mode::Session;
        self.send(Command::OpenTile(AgentSessionId(asid.to_string())));
        self.sync_watch();
        self.persist_tiles();
    }

    /// Add an empty tile that offers a session to pick (the Split action).
    pub fn split(&mut self) {
        if self.tiles.len() >= MAX_TILES {
            return;
        }
        // The inspector describes one session; tiles need the width.
        self.right_open = false;
        self.right.set_open(false);
        if self.tiles.is_empty() {
            let Some(current) = self.selected.clone() else {
                return;
            };
            self.tiles.push(Tile {
                asid: Some(current),
                slot: SessionSlot::default(),
            });
            self.focus = 0;
        }
        self.tiles.push(Tile {
            asid: None,
            slot: SessionSlot::default(),
        });
        self.mode = Mode::Session;
    }

    /// Close tile `i`; one tile left means the pane goes back to one session.
    pub fn close_tile(&mut self, i: usize) {
        if i >= self.tiles.len() {
            return;
        }
        if i == self.focus {
            // Hand focus to a neighbour first so its state comes to the top.
            let next = if i + 1 < self.tiles.len() {
                i + 1
            } else {
                i.saturating_sub(1)
            };
            if next != i {
                self.focus_tile(next);
            }
        }
        let gone = self.tiles.remove(i);
        if self.focus > i {
            self.focus -= 1;
        }
        if let Some(asid) = gone.asid.as_deref() {
            if self.selected.as_deref() != Some(asid) {
                self.timelines.remove(asid);
            }
        }
        if let Some(asid) = gone.asid {
            let text = gone.slot.draft.text.clone();
            let stored = self
                .cfg
                .general
                .drafts
                .get(&asid)
                .map_or("", String::as_str);
            if stored != text {
                self.edit(ConfigEdit::Draft { key: asid, text });
            }
        }
        if self.tiles.len() <= 1 {
            self.tiles.clear();
            self.focus = 0;
            if self.selected.is_none() {
                self.mode = Mode::Home;
            }
        }
        self.pending_tile = None;
        self.menu = None;
        self.sync_watch();
        self.persist_tiles();
    }

    /// Swap tiles `i` and `j` (their places, not their focus).
    pub fn swap_tiles(&mut self, i: usize, j: usize) {
        if i >= self.tiles.len() || j >= self.tiles.len() || i == j {
            return;
        }
        self.tiles.swap(i, j);
        if self.focus == i {
            self.focus = j;
        } else if self.focus == j {
            self.focus = i;
        }
        self.menu = None;
        self.persist_tiles();
    }

    /// Remember the split for the next launch.
    pub fn persist_tiles(&mut self) {
        let asids: Vec<String> = self.tiles.iter().filter_map(|t| t.asid.clone()).collect();
        let split = self.tile_split;
        if asids != self.cfg.ui.tiles || (split - self.cfg.ui.tile_split).abs() > 0.001 {
            self.edit(ConfigEdit::Tiles { asids, split });
        }
    }

    pub fn set_tile_split(&mut self, split: f32) {
        self.tile_split = split.clamp(0.25, 0.75);
    }

    /// Put the saved split back once the listed sessions include it.
    fn restore_tiles(&mut self) {
        if self.tiles_restored || self.demo {
            return;
        }
        if self.cfg.ui.tiles.len() < 2 {
            self.tiles_restored = true;
            return;
        }
        let saved: Vec<String> = self
            .cfg
            .ui
            .tiles
            .iter()
            .filter(|a| self.session_by_id(a).is_some())
            .cloned()
            .collect();
        // Lists arrive agent by agent: wait for every saved tile, or for
        // every agent, before restoring fewer than were saved.
        let all_listed = !self.agents.is_empty()
            && self
                .agents
                .iter()
                .filter(|a| {
                    a.enabled && {
                        use muqun_gateway::platform::discovery::AgentAvailability as A;
                        matches!(a.status, A::Connected | A::Reachable | A::Unknown)
                    }
                })
                .all(|a| self.listed_agents.contains(&a.id));
        if saved.len() < 2 || (saved.len() < self.cfg.ui.tiles.len() && !all_listed) {
            return;
        }
        self.tiles_restored = true;
        self.open_session(&saved[0]);
        for asid in &saved[1..] {
            self.open_in_tile(asid);
        }
        self.focus_tile(0);
    }

    /// The slot of `asid`'s tile when it is open but not focused.
    fn parked_slot(&mut self, asid: &str) -> Option<&mut SessionSlot> {
        let i = self.tile_of(asid)?;
        (i != self.focus || self.selected.as_deref() != Some(asid)).then(|| &mut self.tiles[i].slot)
    }

    // ---- core → UI ---------------------------------------------------------------

    pub fn apply_updates(&mut self, updates: Vec<Update>) {
        if self.demo {
            return;
        }
        for update in updates {
            match update {
                Update::Agents(agents) => {
                    self.on_agents(agents);
                    self.restore_tiles();
                }
                Update::AgentSessions { agent, sessions } => {
                    self.listed_agents.insert(agent.clone());
                    self.sessions.retain(|s| s.agent_id != agent);
                    self.merge_sessions(sessions);
                    if self.error.as_ref().is_some_and(|e| e.unreachable) {
                        self.error = None;
                    }
                    self.restore_tiles();
                }
                Update::SessionCreated(info) => self.merge_sessions(vec![*info]),
                Update::SessionGone(asid) => {
                    self.sessions.retain(|s| s.asid.0 != asid);
                    self.waiting_perms.remove(&asid);
                    self.waiting_forms.remove(&asid);
                    if let Some(i) = self.tile_of(&asid) {
                        if self.in_split() {
                            self.close_tile(i);
                        }
                    }
                    self.timelines.remove(&asid);
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.selected = None;
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
                Update::Detail { asid, result } => self.on_detail(asid, result),
                Update::AccountLimits { agent, limits } => {
                    self.account_limits.insert(agent, limits);
                }
                Update::Earlier {
                    asid,
                    items,
                    exhausted,
                } => {
                    if let Some(tl) = self.timelines.get_mut(&asid) {
                        tl.prepend(items, exhausted);
                    }
                }
                Update::Composer(update) => self.on_composer(*update),
                Update::Theme(update) => self.on_theme(*update),
                Update::Directories(found) => self.directories.extend(found),
                Update::KnownProjects { agent, projects } => {
                    self.known_projects.insert(agent, projects);
                }
                Update::Diff(stat) => {
                    let patch: String = stat
                        .paths
                        .iter()
                        .map(|f| f.patch.as_str())
                        .filter(|p| !p.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let diff = (!patch.is_empty()).then(|| DiffState::new(parse(&patch)));
                    if self.selected.as_deref() == Some(stat.asid.as_str()) {
                        self.diff = diff;
                        self.diff_stat = stat;
                    } else if let Some(slot) = self.parked_slot(&stat.asid.clone()) {
                        slot.diff = diff;
                        slot.diff_stat = stat;
                    }
                }
                Update::Storage(report) => self.storage = Some(report),
                Update::Tasks { rows, default_cwd } => {
                    self.tasks = rows;
                    self.task_cwd = default_cwd;
                }
                Update::Descendants { asid, sessions } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.descendants = sessions;
                    } else if let Some(slot) = self.parked_slot(&asid) {
                        slot.descendants = sessions;
                    }
                }
                Update::SavedPermissions { asid, items } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.saved_permissions = items;
                    } else if let Some(slot) = self.parked_slot(&asid) {
                        slot.saved_permissions = items;
                    }
                }
                Update::Shells(rows) => self.shells = rows,
                Update::Worktrees(rows) => self.worktrees = rows,
                Update::HubMode(mode) => self.hub_label = mode.to_string(),
                Update::Attention(attention) => self.on_attention(&attention),
                Update::Terminals(update) => self.on_terminals(*update),
                Update::Voice(event) => self.on_voice(event),
            }
        }
        if LAYOUT_DIRTY.swap(false, Ordering::AcqRel) {
            self.persist_layout();
        }
        self.sync_tray();
    }

    /// Raise a system notification unless the owner turned them off or is
    /// looking at that session (`model/notify.rs` has the rules and words).
    fn on_attention(&mut self, attention: &crate::model::notify::Attention) {
        // On screen: the focused session, or any session in a split tile.
        let on_screen = crate::platform::notify::window_focused()
            && self.mode == Mode::Session
            && (self.selected.as_deref() == Some(attention.asid.as_str())
                || self.tile_of(&attention.asid).is_some());
        if !self.cfg.notifications.enabled || on_screen {
            return;
        }
        let info = self.session_by_id(&attention.asid);
        let agent = info.map(|i| self.agent_name(&i.agent_id));
        let language = self.cfg.general.language;
        if let Some(notice) =
            crate::model::notify::compose(attention, info, agent.as_deref(), language)
        {
            crate::platform::notify::show(notice);
        }
    }

    fn on_theme(&mut self, update: ThemeUpdate) {
        match update {
            ThemeUpdate::Library(list) => self.theme_library = list,
            ThemeUpdate::Gallery(view) => self.theme_gallery = view,
            ThemeUpdate::Preview { id, path } => {
                self.theme_previews.insert(id, path);
            }
            ThemeUpdate::Job { id, job } => {
                self.theme_jobs.insert(id, job);
            }
            ThemeUpdate::Applied(applied) => {
                let (id, scheme) = (applied.manifest.id.clone(), applied.scheme);
                match crate::theme::apply::activate(applied) {
                    Ok(_) => {
                        self.cfg.appearance.theme_id = id.clone();
                        self.cfg.appearance.scheme = scheme;
                        self.dark = !matches!(scheme, Scheme::Light);
                        self.status = format!("Theme: {id}");
                        self.arm_splash(splash::SplashKind::Theme);
                    }
                    Err(err) => self.toast("Could not apply theme", Some(format!("{err:#}"))),
                }
            }
            ThemeUpdate::Notice(notice) => {
                self.status = notice.message.clone();
                let detail = notice
                    .error
                    .then(|| "Try again from Settings → Themes".to_string());
                self.toast(notice.message, detail);
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
        let first = !self.agents_seen;
        self.agents_seen = true;
        self.agents = agents;
        if first && self.start_cwd.text.is_empty() {
            let cwd = self.cfg.general.initial_cwd(&self.start_agent);
            self.start_cwd = TextEdit::new(cwd.display().to_string());
            if !self.start_agent.is_empty() {
                self.send(Command::LoadProjects {
                    agent: self.start_agent.clone(),
                });
            }
        } else if first && !self.start_agent.is_empty() {
            self.send(Command::LoadProjects {
                agent: self.start_agent.clone(),
            });
            self.refresh_start_catalog();
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
        self.sessions.truncate(SESSIONS_KEPT);
    }

    fn on_snapshot(&mut self, snapshot: muqun_gateway::agents::ports::AgentSessionSnapshot) {
        let asid = snapshot.info.asid.0.clone();
        // A session opened into a tile that is not focused fills that tile.
        if let Some(i) = self.tile_of(&asid) {
            if i != self.focus {
                self.merge_sessions(vec![snapshot.info.clone()]);
                self.timelines
                    .entry(asid.clone())
                    .or_default()
                    .replace(snapshot.timeline);
                self.waiting_perms
                    .insert(asid.clone(), snapshot.permissions);
                self.waiting_forms.insert(asid.clone(), snapshot.forms);
                let queue = InboxItem::list(&snapshot.inbox);
                self.tiles[i].slot.queue = queue;
                return;
            }
        }
        // A session started from an empty tile lands in it.
        if let Some(i) = self.pending_tile.take() {
            if self.tiles.get(i).is_some_and(|t| t.asid.is_none()) {
                self.focus_tile(i);
                self.tiles[i].asid = Some(asid.clone());
                self.sync_watch();
            }
        }
        let fresh = self.selected.as_deref() != Some(asid.as_str());
        self.selected = Some(asid.clone());
        if let Some(tile) = self.tiles.get_mut(self.focus) {
            if tile.asid.as_deref() != Some(asid.as_str()) {
                tile.asid = Some(asid.clone());
                self.sync_watch();
                self.persist_tiles();
            }
        }
        if self.mode == Mode::Home {
            self.mode = Mode::Session;
        }
        self.merge_sessions(vec![snapshot.info.clone()]);
        let count = snapshot.timeline.len();
        self.timelines
            .entry(asid.clone())
            .or_default()
            .replace(snapshot.timeline);
        if fresh {
            self.retain_timelines();
        }
        self.waiting_perms
            .insert(asid.clone(), snapshot.permissions);
        self.waiting_forms.insert(asid.clone(), snapshot.forms);
        self.queue = InboxItem::list(&snapshot.inbox);
        self.status = format!("{count} messages");
        if self.error.as_ref().is_some_and(|e| e.unreachable) {
            self.error = None;
        }
        if fresh {
            self.load_draft(&asid);
            self.session_opened(&snapshot.info);
        }
    }

    fn apply_event(&mut self, event: AgentDomainEvent) {
        let current =
            |s: &Self, asid: &AgentSessionId| s.selected.as_deref() == Some(asid.0.as_str());
        match event {
            AgentDomainEvent::TimelineUpsert {
                asid, mut items, ..
            } => {
                if !self.timelines.contains_key(&asid.0) {
                    return;
                }
                // Same egress step the App gets over HTTP/SSE (`attach_to_event`):
                // fill `image_assets` before the timeline paints.
                let directory = self
                    .sessions
                    .iter()
                    .find(|s| s.asid == asid)
                    .and_then(|s| s.directory.as_deref());
                muqun_gateway::agents::message_images::enrich_timeline(
                    &asid.0, directory, &mut items,
                );
                if let Some(tl) = self.timelines.get_mut(&asid.0) {
                    tl.upsert(items);
                }
            }
            AgentDomainEvent::TimelineRemoved { asid, ids, .. } => {
                if let Some(tl) = self.timelines.get_mut(&asid.0) {
                    tl.remove(&ids);
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
                } else if let Some(slot) = self.parked_slot(&asid.0) {
                    slot.revert_staged = revert.is_some();
                }
            }
            AgentDomainEvent::InboxChanged { asid, items, .. } => {
                if current(self, &asid) {
                    self.queue = InboxItem::list(&items);
                } else if let Some(slot) = self.parked_slot(&asid.0) {
                    slot.queue = InboxItem::list(&items);
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
                self.completions.clear();
                self.start_model = None;
                self.start_mode = None;
                self.refresh_start_catalog();
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
            UiAction::ThemeFilePicked(path) => self.send(Command::ThemeImportFile(path)),
            UiAction::ViewerLoaded { path, loaded } => self.viewer_loaded(&path, *loaded),
            UiAction::FileSaved(result) => self.file_saved(result),
            UiAction::VoiceKey(store) => self.voice_form.key_saved = store,
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
            ComposerUpdate::Catalog {
                agent,
                directory,
                view,
            } => {
                let session = self
                    .current()
                    .is_some_and(|i| i.agent_id == agent && i.directory == directory);
                let typed = self.start_cwd.text.trim();
                let start =
                    agent == self.start_agent && directory.as_deref().unwrap_or("") == typed;
                if start {
                    self.start_catalog = Some(view.clone());
                }
                if session || !start {
                    self.catalog = Some(view);
                }
            }
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
                    self.persist_draft(slot);
                    self.staged.clear();
                    self.status = "Sent".into();
                } else {
                    self.status = "Send failed".into();
                }
            }
            ComposerUpdate::Inbox { asid, items } => {
                if self.selected.as_deref() == Some(asid.as_str()) {
                    self.queue = items;
                } else if let Some(slot) = self.parked_slot(&asid) {
                    slot.queue = items;
                }
            }
            ComposerUpdate::Branch { cwd, info } => {
                self.branches.insert(cwd, info);
            }
            ComposerUpdate::Peers { peers } => self.peers = peers,
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

/// Shared page chrome: centered column, title, optional subtitle.
pub fn page_frame(k: &Kit, key: &str, title: &str, subtitle: Option<&str>) -> El {
    page_frame_w(k, key, title, subtitle, 760.0)
}

/// Like [`page_frame`], with an explicit content width (Themes uses a wider rail).
pub fn page_frame_w(k: &Kit, key: &str, title: &str, subtitle: Option<&str>, max_w: f32) -> El {
    let title_font = rune::TextStyle {
        size: 26.0,
        line_height: 32.0,
        ..k.ty.ui_medium
    };
    let mut head = rune::col().key("page-head").w_full().gap(4.0).child(
        rune::text(title.to_string())
            .font(title_font)
            .color(k.c.text_base)
            .key("page-title"),
    );
    if let Some(sub) = subtitle.filter(|s| !s.is_empty()) {
        head = head.child(
            rune::text(sub.to_string())
                .font(k.ty.body)
                .color(k.c.text_muted)
                .key("page-subtitle"),
        );
    }
    rune::col()
        .key(key.to_string())
        .w_full()
        .max_w(max_w)
        .gap(18.0)
        .px(28.0)
        .pt(28.0)
        .pb(40.0)
        .child(head)
}

/// A page's stage: the scroll column centred over a soft accent light, so
/// Inbox, Tasks and Settings share Home's atmosphere.
pub fn page_stage(s: &UiState, k: &Kit, key: &str, page: El) -> El {
    let look = s.look();
    let mut stage = rune::el()
        .key(format!("{key}-stage"))
        .w_full()
        .h_full()
        .min_h(0.0);
    if look.glass {
        stage = stage.child(look.light(
            &format!("{key}-light"),
            [0.5, -0.1],
            [700.0, 360.0],
            k.c.bg_accent,
        ));
    }
    stage.child(
        rune::col()
            .key(format!("{key}-scroll"))
            .w_full()
            .h_full()
            .items_center()
            .child(page)
            .scroll(rune::ScrollSpec {
                stick_bottom: false,
            })
            .scroll_bar(k),
    )
}

pub fn title_of(info: &AgentSessionInfo) -> String {
    if info.title.trim().is_empty() {
        info.asid.0.clone()
    } else {
        info.title.clone()
    }
}

/// What a session is doing, as the shell shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Working,
    NeedsYou,
    Failed,
    Idle,
}

impl UiState {
    pub fn look(&self) -> Look {
        Look::of(&self.cfg.appearance, self.dark)
    }

    pub fn scheme(&self) -> ColorScheme {
        if self.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        }
    }

    /// Whether a session has a permission or form waiting on the owner.
    pub fn session_waiting(&self, asid: &str) -> bool {
        self.waiting_perms.get(asid).is_some_and(|p| !p.is_empty())
            || self.waiting_forms.get(asid).is_some_and(|f| !f.is_empty())
    }

    pub fn activity(&self, info: &AgentSessionInfo) -> Activity {
        if self.session_waiting(&info.asid.0) {
            Activity::NeedsYou
        } else {
            match info.status {
                AgentSessionStatus::Busy | AgentSessionStatus::Retry => Activity::Working,
                AgentSessionStatus::Failed => Activity::Failed,
                _ => Activity::Idle,
            }
        }
    }

    /// Sessions working right now, newest first.
    pub fn running(&self) -> Vec<&AgentSessionInfo> {
        self.sessions
            .iter()
            .filter(|i| self.activity(i) == Activity::Working)
            .collect()
    }

    /// The folder's short name for a session (`gateway` for `/home/me/gateway`).
    pub fn folder_name(info: &AgentSessionInfo) -> Option<String> {
        let dir = info.directory.as_deref()?.trim_end_matches('/');
        let name = dir.rsplit('/').next().unwrap_or(dir);
        (!name.is_empty()).then(|| name.to_string())
    }

    /// The checkout's branch for a session's folder, when the core resolved it.
    pub fn branch_of(&self, info: &AgentSessionInfo) -> Option<String> {
        let dir = info.directory.as_deref()?;
        self.directories.get(dir).and_then(|d| d.branch.clone())
    }
}

/// Milliseconds since the epoch, now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `updated_ms` as a short relative time ("now", "4m", "3h", "2d").
pub fn ago(updated_ms: u64, now: u64, language: crate::config::Language) -> String {
    let zh = language == crate::config::Language::ZhCn;
    if updated_ms == 0 {
        return String::new();
    }
    let secs = now.saturating_sub(updated_ms) / 1000;
    match secs {
        0..=59 => tr(language, "now", "刚刚"),
        60..=3599 => format!("{}{}", secs / 60, if zh { " 分钟前" } else { "m" }),
        3600..=86_399 => format!("{}{}", secs / 3600, if zh { " 小时前" } else { "h" }),
        _ => format!("{}{}", secs / 86_400, if zh { " 天前" } else { "d" }),
    }
}

/// A small status pill: a dot (or spinner) and a word, in the state's colour.
pub fn activity_pill(
    k: &Kit,
    look: &Look,
    activity: Activity,
    language: crate::config::Language,
) -> El {
    let (color, word) = match activity {
        Activity::Working => (k.c.text_accent, tr(language, "Working", "工作中")),
        Activity::NeedsYou => (k.c.warning.icon, tr(language, "Needs you", "等你处理")),
        Activity::Failed => (k.c.danger.icon, tr(language, "Failed", "失败")),
        Activity::Idle => (k.c.text_faint, tr(language, "Idle", "空闲")),
    };
    let mark = match activity {
        Activity::Working => rune_kit::Spinner::new().size(10.0).color(color).build(k),
        Activity::NeedsYou => look.pulse(rune::el().size(7.0, 7.0).radius(3.5).fill(color)),
        _ => rune::el().size(7.0, 7.0).radius(3.5).fill(color),
    };
    rune::row()
        .key("activity")
        .h(20.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(10.0)
        .fill(color.alpha(if activity == Activity::Idle {
            0.08
        } else {
            0.14
        }))
        .role("status")
        .label(word.clone())
        .child(mark.a11y_hidden())
        .child(
            rune::text(word)
                .font(k.ty.small_medium)
                .color(color)
                .a11y_hidden(),
        )
}

use copy::tr;

pub fn on_key(s: &mut UiState, ev: &KeyEvent, _tx: &mut Tx) -> bool {
    if ev.is_command('k') && !ev.mods.shift && !ev.mods.alt {
        s.toggle_palette();
        return true;
    }
    if ev.is_command(' ') && ev.mods.shift && !ev.mods.alt && s.mode == Mode::Session {
        s.toggle_voice();
        return true;
    } else if s.voice.is_some() && ev.key == rune::Key::Escape {
        s.cancel_voice();
        return true;
    } else if is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.is_command('t') && ev.mods.shift && !ev.mods.alt {
        s.settings_tab = SettingsTab::Themes;
        s.go(Mode::Settings);
        return true;
    }
    if ev.mods.shift || ev.mods.alt {
        return false;
    }
    if ev.is_command(',') {
        s.go(Mode::Settings);
        return true;
    }
    if ev.is_command('n') {
        s.go(Mode::Home);
        s.start_draft.focused = true;
        return true;
    }
    if ev.is_command('f') {
        s.sidebar.set_open(true);
        s.search.focused = true;
        return true;
    }
    if ev.is_command('\\') {
        s.split();
        return true;
    }
    if ev.is_command('w') && s.in_split() {
        s.close_tile(s.focus);
        return true;
    }
    for (n, c) in ['1', '2', '3', '4'].into_iter().enumerate() {
        if ev.is_command(c) {
            if s.in_split() && n < s.tiles.len() {
                s.focus_tile(n);
                return true;
            }
            return false;
        }
    }
    false
}

pub fn view(s: &UiState, cx: &mut Cx<UiState>) -> El {
    // Desktop CI smoke greps this once the first frame builds (xvfb / lavapipe).
    static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    if FIRST_FRAME.swap(false, std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!("build_ui");
    }
    let dark = s.is_dark(cx.system_dark());
    let scheme = if dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    let mut k =
        crate::theme::apply::palette(scheme).map_or_else(|| Kit::neutral(dark), |p| p.kit());
    crate::theme::look::tune_kit(&mut k, &s.cfg.appearance);
    cx.focus_ring(2.0, 1.5, k.c.border_focus);
    if s.cfg.appearance.reduced_motion {
        cx.reduced_motion(true);
    }
    let mut layer: Vec<El> = Vec::new();
    let body = shell::view(s, cx, &k, &mut layer);
    if let Some(zoom) = timeline::overlay(s, &k) {
        layer.push(zoom);
    }
    welcome::mount(s, &k, &mut layer);
    if let (Some(splash), Some(images)) = (s.splash.as_ref(), s.images.as_ref()) {
        let hub_ready = !s.hub_label.is_empty();
        let loading = match splash.kind {
            splash::SplashKind::Boot => {
                !splash::Splash::data_ready(splash.kind, s.agents_seen, hub_ready)
            }
            splash::SplashKind::Theme => false,
        };
        if let Some(stage) = splash.view(
            images,
            &k,
            &s.look(),
            scheme,
            s.cfg.general.language,
            loading,
        ) {
            layer.push(stage);
        }
    }
    terminals::page_visible(s.mode);
    viewer::mount(s, &k, cx.env().scale, &mut layer);
    palette::mount(s, &k, &mut layer);
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
                // A pane grip was touched: look at the widths once the
                // quiet frame arrives (a drag in progress re-arms the flag).
                if LAYOUT_DIRTY.swap(false, Ordering::AcqRel) {
                    handle.update(|s, _| s.persist_layout());
                }
                continue;
            }
            handle.update(move |s, _| s.apply_updates(batch));
        })
        .expect("inbox pump");
}
