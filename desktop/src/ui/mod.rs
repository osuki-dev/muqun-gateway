//! Muqun Desktop UI: rune-kit chrome and controls over the core's [`Update`]s.
//!
//! `UiState` is the main-thread model. [`apply_updates`](UiState::apply_updates) folds
//! every core update into it, each page sends [`Command`]s back, and [`view`] lays the
//! shell out from it.

mod agent_marks;
mod agents;
mod art;
mod composer;
mod composer_row;
pub mod copy;
mod dwindle;
mod echo;
pub mod fx;
mod home;
pub mod i18n;
mod icons;
mod inbox;
mod inspector;
pub mod keymap;
mod labels;
pub mod pair_computer;
mod palette;
mod panes;
mod parts;
mod running;
pub mod scenes;
mod search;
mod sessions;
mod settings;
mod shell;
mod shortcuts;
mod splash;
mod tasks;
mod terminal_panel;
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
use rune_kit::shell::PaneState;
use rune_kit::{Kit, ToastRegion};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
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

/// How many gallery themes the Themes tab lists before "Show more".
pub(crate) const GALLERY_PAGE: usize = 36;

/// A pane's grip or toggle was touched: the width may have changed and
/// [`UiState::persist_layout`] should look (set from the pane access
/// functions, read by the inbox pump).
static LAYOUT_DIRTY: AtomicBool = AtomicBool::new(false);

fn sidebar_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.sidebar
}

fn inspect_pane(s: &mut UiState) -> &mut PaneState {
    &mut s.inspect_pane
}

pub use parts::pane::{Inspect, RowMenu};

fn right_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.right
}

fn terminal_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.term.pane
}

/// The one open dropdown (or row menu), if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenMenu {
    /// The session composer's model, effort, branch menus.
    Model,
    Effort,
    Branch,
    /// The session composer's permission mode menu, and its overflow
    /// (what a narrow composer folds away).
    Mode,
    ComposerMore,
    /// The start page's model, mode and branch menus.
    StartModel,
    StartMode,
    StartBranch,
    /// The start page's other agents.
    StartAgents,
    /// The start page's folder picker.
    StartFolder,
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
    /// A dropdown on a Settings page (by its key).
    Setting(String),
    /// The sidebar's computer picker.
    ComputerPicker,
    /// The tasks page's computer filter picker.
    TasksComputerPicker,
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
    pub diff_active_file: usize,
    pub revert_staged: bool,
    pub catalog: Option<Arc<CatalogView>>,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
}

/// One pane of the split session view. `asid` is `None` while the tile
/// waits for a session to be picked into it.
#[derive(Default)]
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

/// Cached per-computer workspace state so switching machines is instant.
#[derive(Default)]
pub struct ComputerData {
    pub sessions: Vec<AgentSessionInfo>,
    pub directories: HashMap<String, DirInfo>,
    pub known_projects: HashMap<String, Vec<AgentProject>>,
    pub agent_projects: HashMap<String, Vec<AgentProject>>,
    pub agents: Vec<AgentDiscoveryInfo>,
    pub selected: Option<String>,
    pub timelines: HashMap<String, timeline::SessionTimeline>,
    pub tasks: Vec<TaskRow>,
    pub staged: Vec<StagedFile>,
    pub branches: HashMap<String, BranchInfo>,
    pub tiles: Vec<Tile>,
    pub focus: usize,
    pub mode: Mode,
}

/// In-session timeline search state (Ctrl+F).
#[derive(Clone, Debug, Default)]
pub struct TimelineSearch {
    pub active: bool,
    pub query: TextEdit,
    pub matches: Vec<String>,
    pub active_index: usize,
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
    /// The timeline row open in the right-hand detail pane (`parts::pane`).
    pub inspect: Option<Inspect>,
    /// The detail pane's width.
    pub inspect_pane: PaneState,
    /// How wide the right-hand panes may be in this window (`panes.rs`).
    pub fits: panes::Fits,
    /// A timeline row's context menu, where it was asked for.
    pub row_menu: Option<RowMenu>,
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
    /// Per-computer cached state for instant switching between workspaces.
    pub computer_data: HashMap<String, ComputerData>,
    /// Esc dismissed a composer popup at this query start; forgotten when the
    /// trigger leaves the draft (App `dismissedAt`).
    pub popup_dismissed_at: Option<usize>,
    /// The composer popup's keyboard row: (query start, term, row).
    pub popup_active: Option<(usize, String, usize)>,
    /// File paths inserted as `@` mentions (App `fileTokens`), for highlight /
    /// atomic delete of typed paths still standing in the draft.
    pub mention_file_tokens: Vec<String>,
    /// Whether a file drag from the OS is hovering over an attachment drop zone.
    pub drop_hover: bool,
    /// Previous draft text + caret, for App-style atomic mention backspace.
    pub draft_snap: String,
    pub draft_snap_caret: usize,
    pub auto_approve: HashMap<String, bool>,
    pub next_token: u64,
    pub pending_submit: Option<(u64, Slot)>,
    /// Prompts shown before the gateway's row for them arrives, by session
    /// (`ui/echo.rs`).
    pub echoes: HashMap<String, Vec<echo::Echo>>,
    /// Bumped when an echo comes, changes or goes (the timeline's version).
    pub echo_rev: u64,
    /// Echoed submits the gateway has not answered yet (their echo may
    /// already be gone, its row arrived first).
    pub echo_tokens: HashSet<u64>,

    pub diff_stat: DiffStat,
    pub diff: Option<DiffState>,
    pub show_diff: bool,
    pub diff_active_file: usize,
    pub revert_staged: bool,

    pub tasks: Vec<TaskRow>,
    pub task_cwd: Option<String>,
    pub task_goal: TextEdit,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
    pub shells: Vec<ShellRow>,
    pub worktrees: Vec<WorktreeRow>,

    pub search: TextEdit,
    pub pairing_target: Option<String>,
    pub pairing_request_id: Option<String>,
    pub pairing_code_input: TextEdit,
    pub pairing_token_input: TextEdit,
    pub pairing_url_input: TextEdit,
    pub pairing_manual_token_mode: bool,
    pub pairing_transport_required: bool,
    pub pairing_busy: bool,
    pub pairing_error: Option<String>,
    /// The sidebar's "In messages" hits (`ui/search.rs`).
    pub content: search::ContentSearch,
    /// A hit being opened: its row is revealed once its session is loaded.
    pub jump: Option<search::PendingJump>,
    /// Why the link to the gateway is down, while it is.
    pub hub_down: Option<String>,
    /// Inline rename of a session row (its asid) and the text being typed.
    pub rename_asid: Option<String>,
    pub rename_edit: TextEdit,
    /// Inline rename of a project row (its id) and the text being typed.
    pub rename_project: Option<String>,
    pub project_edit: TextEdit,
    pub open_projects: HashMap<String, bool>,
    /// When this window first saw each working session working (`ui/running.rs`).
    pub busy_since: HashMap<String, u64>,
    /// The start card's folder picker filter (`ui/home.rs`).
    pub folder_query: TextEdit,
    pub full_projects: HashSet<String>,

    /// Pending prompts per session, from snapshots and events.
    pub waiting_perms: HashMap<String, Vec<PermissionRequest>>,
    pub waiting_forms: HashMap<String, Vec<FormRequest>>,
    /// The Inbox page's selection and when each prompt first arrived.
    pub inbox_view: inbox::InboxView,
    /// The Tasks page's selection, filters and task histories.
    pub tasks_view: tasks::TasksView,
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
    /// Settings → Themes: the search over both grids.
    pub theme_query: TextEdit,
    pub theme_tab: settings::ThemeTab,
    pub gallery_limit: usize,
    pub toasts: Vec<ToastItem>,
    toast_seq: u64,
    /// Theme / cover decode cache (wakes the app when an image is ready).
    pub images: Option<ImageCache>,
    /// Launch / theme-switch splash while the core warms up.
    pub splash: Option<splash::Splash>,
    /// Previous session id before last switch (for Ctrl+Tab switching).
    pub last_selected: Option<String>,
    /// Prompt history for up/down recall in the composer.
    pub prompt_history: Vec<String>,
    pub prompt_history_cursor: Option<usize>,
    pub prompt_history_draft_stash: Option<String>,
    /// Timeline in-session search state (Ctrl+F in session mode).
    pub timeline_search: TimelineSearch,
    /// First agents discovery finished (empty list still counts).
    pub agents_seen: bool,
    /// A screenshot scene is showing: live core updates are ignored.
    pub demo: bool,
    /// What the desktop keeps on disk (Settings → Storage), once scanned.
    pub storage: Option<crate::model::storage::StorageReport>,
    /// The terminal panel on the right (`ui/terminal_panel.rs`).
    pub term: terminal_panel::TermPanel,
    /// What the status icon last showed (`ui/tray.rs`).
    tray_shown: Option<(crate::config::Language, Vec<(String, String)>)>,
    /// The command palette (`ui/palette.rs`), when open.
    pub palette: Option<palette::Palette>,
    /// The keyboard shortcuts sheet (`ui/shortcuts.rs`), when open.
    pub keys_sheet: Option<shortcuts::Sheet>,
    /// Dictation in progress (`ui/voice.rs`).
    pub voice: Option<voice::Dictation>,
    /// In-app self-update status.
    pub update_state: crate::model::inbox::UpdateState,
    /// Settings → Voice's fields.
    pub voice_form: voice::VoiceForm,
    /// The file viewer overlay ([`UiState::open_file`]).
    pub viewer: Option<viewer::Viewer>,
    /// Sessions open side by side (empty when one session fills the pane).
    pub tiles: Vec<Tile>,
    /// The tile the keyboard and the composer belong to.
    pub focus: usize,
    /// How the tiles sit (`dwindle.rs`); [`UiState::tile_layout`] reads it.
    pub tile_tree: crate::config::TileNode,
    /// An empty tile waiting for the next opened session.
    pub pending_tile: Option<usize>,
    /// The split's size on screen, for splitting and the gaps' drags.
    pub tile_grid: [f32; 2],
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
                .range(240.0, panes::UNBOUNDED)
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
            inspect: None,
            inspect_pane: PaneState::new(440.0).range(320.0, panes::UNBOUNDED),
            fits: panes::Fits::default(),
            row_menu: None,
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
            computer_data: HashMap::new(),
            popup_dismissed_at: None,
            popup_active: None,
            mention_file_tokens: Vec::new(),
            drop_hover: false,
            draft_snap: String::new(),
            draft_snap_caret: 0,
            auto_approve: HashMap::new(),
            next_token: 1,
            pending_submit: None,
            echoes: HashMap::new(),
            echo_rev: 0,
            echo_tokens: HashSet::new(),
            diff_stat: DiffStat::default(),
            diff: None,
            show_diff: false,
            diff_active_file: 0,
            revert_staged: false,
            tasks: Vec::new(),
            task_cwd: None,
            task_goal: TextEdit::new(""),
            descendants: Vec::new(),
            saved_permissions: Vec::new(),
            shells: Vec::new(),
            worktrees: Vec::new(),
            search: TextEdit::new(""),
            pairing_target: None,
            pairing_request_id: None,
            pairing_code_input: TextEdit::new(""),
            pairing_token_input: TextEdit::new(""),
            pairing_url_input: TextEdit::new(""),
            pairing_manual_token_mode: false,
            pairing_transport_required: false,
            pairing_busy: false,
            pairing_error: None,
            content: Default::default(),
            jump: None,
            hub_down: None,
            rename_asid: None,
            rename_edit: TextEdit::new(""),
            rename_project: None,
            project_edit: TextEdit::new(""),
            open_projects: HashMap::new(),
            busy_since: HashMap::new(),
            folder_query: TextEdit::new(""),
            full_projects: HashSet::new(),
            waiting_perms: HashMap::new(),
            waiting_forms: HashMap::new(),
            inbox_view: Default::default(),
            tasks_view: Default::default(),
            form_inputs: HashMap::new(),
            agent_inputs: HashMap::new(),
            agent_errors: HashMap::new(),
            cwd_input,
            theme_library: Arc::new(Vec::new()),
            theme_gallery: Arc::new(GalleryView::default()),
            theme_previews: HashMap::new(),
            theme_jobs: HashMap::new(),
            theme_url: TextEdit::new(""),
            theme_query: TextEdit::new(""),
            theme_tab: settings::ThemeTab::Installed,
            gallery_limit: GALLERY_PAGE,
            toasts: Vec::new(),
            toast_seq: 0,
            images: None,
            splash: None,
            last_selected: None,
            prompt_history: Vec::new(),
            prompt_history_cursor: None,
            prompt_history_draft_stash: None,
            timeline_search: TimelineSearch::default(),
            agents_seen: false,
            demo: false,
            storage: None,
            viewer: None,
            voice: None,
            update_state: crate::model::inbox::UpdateState::Idle,
            palette: None,
            keys_sheet: None,
            tray_shown: None,
            voice_form: voice::VoiceForm::new(&cfg.voice),
            term: terminal_panel::TermPanel::new(cfg.ui.terminal()),
            tiles: Vec::new(),
            focus: 0,
            tile_tree: crate::config::TileNode::Leaf(0),
            pending_tile: None,
            tile_grid: [0.0; 2],
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
        self.term.waker = Some(waker.clone());
        let cache = ImageCache::new(waker);
        let scheme = if self.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        };
        // Start decoding what the first frames draw.
        for art in [
            crate::theme::apply::launch(scheme),
            crate::theme::apply::backdrop(scheme, true),
            crate::theme::apply::home_artwork(scheme),
        ]
        .into_iter()
        .flatten()
        {
            let _ = cache.load(art.bytes);
        }
        self.images = Some(cache);
    }

    /// Keep the window waking while the splash is animating.
    /// Every frame Rune asks: the splash and a recording's waveform animate.
    pub fn poll_frame(&mut self, t: f64, tx: &mut Tx) -> bool {
        let voice = self.poll_voice();
        let fit = self.fit_panes();
        let term = self.poll_terminal(t, tx);
        let search = self.poll_search(tx);
        let echoes = self.poll_echoes(tx);
        self.poll_splash(t, tx) | voice | fit | term | search | echoes
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

    pub fn current_computer(&self) -> &str {
        if self.cfg.current_computer.is_empty() {
            "local"
        } else {
            &self.cfg.current_computer
        }
    }

    pub fn computer_name(&self, id: &str) -> String {
        let language = self.cfg.general.language;
        if id == "all" {
            copy::tr(language, "All computers")
        } else if id == "local" || id == "self" {
            copy::tr(language, "This computer")
        } else if let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.server_id == id || p.name == id)
        {
            peer.name.clone()
        } else if let Some(p) = self.cfg.computers.get(id) {
            if !p.name.is_empty() {
                p.name.clone()
            } else {
                id.to_string()
            }
        } else {
            id.to_string()
        }
    }

    #[allow(dead_code)]
    pub fn computer_color(&self, id: &str) -> rune::Color {
        if let Some(accent) = self.cfg.accent_for(id) {
            crate::theme::agents::computer_color(accent, self.dark)
        } else {
            crate::theme::agents::computer_color(id, self.dark)
        }
    }

    pub fn all_computers(&self) -> Vec<crate::hub::PeerMention> {
        let mut list = self.peers.clone();
        let curr = self.current_computer();
        if !list
            .iter()
            .any(|p| p.server_id == "self" || p.server_id == "local")
        {
            list.insert(
                0,
                crate::hub::PeerMention {
                    server_id: "local".into(),
                    name: self.computer_name("local"),
                    reachable: true,
                    current: curr == "local" || curr == "self",
                    url: None,
                },
            );
        }
        for (id, profile) in &self.cfg.computers {
            if !list.iter().any(|p| &p.server_id == id) {
                list.push(crate::hub::PeerMention {
                    server_id: id.clone(),
                    name: if profile.name.is_empty() {
                        id.clone()
                    } else {
                        profile.name.clone()
                    },
                    reachable: true,
                    current: curr == id,
                    url: profile.url.clone(),
                });
            }
        }
        list
    }

    pub fn is_computer_paired(&self, computer_id: &str) -> bool {
        if computer_id == "local" || computer_id == "self" || computer_id == "all" {
            return true;
        }
        self.cfg
            .computers
            .get(computer_id)
            .and_then(|p| p.token.as_ref())
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn peer_by_id(&self, computer_id: &str) -> Option<&PeerMention> {
        self.peers
            .iter()
            .find(|p| p.server_id == computer_id || p.name == computer_id)
    }

    pub fn peer_url(&self, computer_id: &str) -> Option<String> {
        self.peer_by_id(computer_id)
            .and_then(|p| p.url.clone())
            .or_else(|| {
                self.cfg
                    .computers
                    .get(computer_id)
                    .and_then(|p| p.url.clone())
            })
    }

    pub fn computer_of_session(&self, asid: &str) -> Option<String> {
        let curr = self.current_computer();
        if curr != "all" && self.sessions.iter().any(|s| s.asid.0 == asid) {
            return Some(curr.to_string());
        }
        for (cid, cdata) in &self.computer_data {
            if cdata.sessions.iter().any(|s| s.asid.0 == asid) {
                return Some(cid.clone());
            }
        }
        if self.sessions.iter().any(|s| s.asid.0 == asid) {
            return Some("local".to_string());
        }
        None
    }

    pub fn open_pairing(&mut self, target_id: String) {
        let default_url = self.peer_url(&target_id).unwrap_or_default();
        self.pairing_target = Some(target_id);
        self.pairing_request_id = None;
        self.pairing_busy = false;
        self.pairing_error = None;
        self.pairing_manual_token_mode = false;
        self.pairing_code_input.clear();
        self.pairing_code_input.focused = true;
        self.pairing_token_input.clear();
        self.pairing_url_input = TextEdit::new(default_url);
    }

    pub fn close_pairing(&mut self) {
        self.pairing_target = None;
        self.pairing_request_id = None;
        self.pairing_busy = false;
        self.pairing_error = None;
        self.pairing_manual_token_mode = false;
        self.pairing_code_input.clear();
        self.pairing_code_input.focused = true;
        self.pairing_token_input.clear();
    }

    pub fn switch_computer(&mut self, id: String) {
        let prev = self.current_computer().to_string();
        if prev != id {
            let data = ComputerData {
                sessions: std::mem::take(&mut self.sessions),
                directories: std::mem::take(&mut self.directories),
                known_projects: std::mem::take(&mut self.known_projects),
                agent_projects: std::mem::take(&mut self.agent_projects),
                agents: std::mem::take(&mut self.agents),
                selected: self.selected.take(),
                timelines: std::mem::take(&mut self.timelines),
                tasks: std::mem::take(&mut self.tasks),
                staged: std::mem::take(&mut self.staged),
                branches: std::mem::take(&mut self.branches),
                tiles: std::mem::take(&mut self.tiles),
                focus: self.focus,
                mode: self.mode,
            };
            self.computer_data.insert(prev, data);

            if id == "all" {
                let mut all_sess = Vec::new();
                for cdata in self.computer_data.values() {
                    for sess in &cdata.sessions {
                        if !all_sess
                            .iter()
                            .any(|s: &AgentSessionInfo| s.asid == sess.asid)
                        {
                            all_sess.push(sess.clone());
                        }
                    }
                }
                all_sess.sort_by_key(|a| std::cmp::Reverse(a.updated_ms));
                self.sessions = all_sess;
                self.tiles = vec![Tile::default()];
                self.focus = 0;
                self.selected = None;
                self.mode = Mode::Home;
            } else if let Some(restored) = self.computer_data.remove(&id) {
                self.sessions = restored.sessions;
                self.directories = restored.directories;
                self.known_projects = restored.known_projects;
                self.agent_projects = restored.agent_projects;
                self.agents = restored.agents;
                self.selected = restored.selected;
                self.timelines = restored.timelines;
                self.tasks = restored.tasks;
                self.staged = restored.staged;
                self.branches = restored.branches;
                self.tiles = if restored.tiles.is_empty() {
                    vec![Tile::default()]
                } else {
                    restored.tiles
                };
                self.focus = restored.focus;
                self.mode = restored.mode;
            } else {
                self.tiles = vec![Tile::default()];
                self.focus = 0;
                self.selected = None;
                self.mode = Mode::Home;
            }
        }

        self.cfg.current_computer = id.clone();
        self.edit(ConfigEdit::CurrentComputer(id.clone()));
        self.agent_inputs.clear();
        self.agent_errors.clear();
        self.pairing_request_id = None;
        self.pairing_busy = false;
        self.pairing_error = None;
        self.pairing_code_input.clear();
        self.pairing_token_input.clear();
        let default_url = self.peer_url(&id).unwrap_or_default();
        self.pairing_url_input = TextEdit::new(default_url);

        if id != "all" {
            let theme_id = self.cfg.theme_for(&id).to_string();
            if theme_id != self.cfg.appearance.theme_id {
                self.cfg.appearance.theme_id = theme_id.clone();
                self.send(Command::ThemeApply(theme_id, self.cfg.appearance.scheme));
            }
        }
        for peer in &mut self.peers {
            peer.current = peer.server_id == id || (id == "local" && peer.server_id == "self");
        }

        if id != "all" {
            self.send(Command::SwitchComputer(id.clone()));
            self.send(Command::Refresh);
        }

        let name = self.computer_name(&id);
        self.toast(
            format!(
                "{} {name}",
                copy::t(self.cfg.general.language, "Switched to")
            ),
            None,
        );
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

    /// Show session `asid`. In split view it joins as a new pane (or takes
    /// focus where it already is); otherwise it fills the main card.
    pub fn open_session(&mut self, asid: &str) {
        if self.in_split() && self.tile_of(asid).is_none() {
            crate::platform::notify::withdraw(asid);
            self.open_in_tile(asid);
            return;
        }
        self.open_here(asid);
    }

    /// Show session `asid` in the focused pane (the main card outside split
    /// view), in place of what it showed.
    fn open_here(&mut self, asid: &str) {
        crate::platform::notify::withdraw(asid);
        if let Some(i) = self.tile_of(asid) {
            if i != self.focus {
                self.focus_tile(i);
                return;
            }
        }
        let prev = self.selected.clone();
        let changed = self.selected.as_deref() != Some(asid);
        if changed && prev.as_deref() != Some(asid) && prev.is_some() {
            self.last_selected = prev;
        }
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
            self.timelines.remove(asid);
            self.retain_timelines();
            self.queue.clear();
            self.staged.clear();
            self.diff_stat = DiffStat::default();
            self.diff = None;
            self.diff_active_file = 0;
            self.show_diff = false;
        }
        self.status = format!("Opening {asid}");
        self.send(Command::OpenSession(AgentSessionId(asid.to_string())));
        self.send(Command::Compose(
            crate::model::compose::ComposeCommand::ListPeers,
        ));
    }

    /// Toggle to the most recent previous session (Ctrl+Tab).
    pub fn toggle_recent_session(&mut self) {
        if let Some(last) = self.last_selected.clone() {
            self.open_session(&last);
        }
    }

    /// Record a non-empty submitted prompt into history.
    pub fn record_prompt_history(&mut self, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        if self.prompt_history.last().map(|s| s.as_str()) != Some(t) {
            self.prompt_history.push(t.to_string());
        }
        self.prompt_history_cursor = None;
        self.prompt_history_draft_stash = None;
    }

    /// Navigate backward in prompt history (Up arrow).
    pub fn prompt_history_prev(&mut self) -> bool {
        if self.prompt_history.is_empty() {
            return false;
        }
        let current_cursor = self.prompt_history_cursor;
        let new_cursor = match current_cursor {
            None => {
                self.prompt_history_draft_stash = Some(self.draft.text.clone());
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(idx) => idx - 1,
        };
        self.prompt_history_cursor = Some(new_cursor);
        let text = self.prompt_history[new_cursor].clone();
        let len = text.len();
        self.draft.text = text;
        self.draft.set_caret(len);
        true
    }

    /// Navigate forward in prompt history (Down arrow).
    pub fn prompt_history_next(&mut self) -> bool {
        let Some(idx) = self.prompt_history_cursor else {
            return false;
        };
        if idx + 1 < self.prompt_history.len() {
            let new_cursor = idx + 1;
            self.prompt_history_cursor = Some(new_cursor);
            let text = self.prompt_history[new_cursor].clone();
            let len = text.len();
            self.draft.text = text;
            self.draft.set_caret(len);
            true
        } else {
            self.prompt_history_cursor = None;
            let text = self.prompt_history_draft_stash.take().unwrap_or_default();
            let len = text.len();
            self.draft.text = text;
            self.draft.set_caret(len);
            true
        }
    }

    /// Reset prompt history cursor when user types or submits, restoring draft if stashed.
    pub fn reset_prompt_history_cursor(&mut self) {
        if let Some(stash) = self.prompt_history_draft_stash.take() {
            let len = stash.len();
            self.draft.text = stash;
            self.draft.set_caret(len);
        }
        self.prompt_history_cursor = None;
    }

    /// Toggle floating in-session timeline search (Ctrl+F).
    pub fn toggle_timeline_search(&mut self) {
        self.timeline_search.active = !self.timeline_search.active;
        if self.timeline_search.active {
            self.timeline_search.query.focused = true;
            if let Some(asid) = self.selected.clone() {
                self.update_timeline_search_matches(&asid);
            }
        }
    }

    /// Close floating in-session timeline search.
    pub fn close_timeline_search(&mut self) {
        self.timeline_search.active = false;
        self.timeline_search.matches.clear();
        self.timeline_search.active_index = 0;
        if self.mode == Mode::Session {
            self.draft.focused = true;
        }
    }

    /// Update matching items in the current session for the timeline search query.
    pub fn update_timeline_search_matches(&mut self, asid: &str) {
        let q = self.timeline_search.query.text.trim().to_lowercase();
        self.timeline_search.matches.clear();
        self.timeline_search.active_index = 0;
        if q.is_empty() {
            return;
        }
        if let Some(tl) = self.timelines.get(asid) {
            for item in &tl.items {
                let matches = match &item.part {
                    muqun_gateway::agents::AgentPart::Text { text, .. } => {
                        text.to_lowercase().contains(&q)
                    }
                    muqun_gateway::agents::AgentPart::Reasoning { text, .. } => {
                        text.to_lowercase().contains(&q)
                    }
                    muqun_gateway::agents::AgentPart::Tool(tool) => {
                        tool.name.to_lowercase().contains(&q)
                            || tool
                                .title
                                .as_ref()
                                .is_some_and(|t| t.to_lowercase().contains(&q))
                            || tool.input.to_string().to_lowercase().contains(&q)
                            || tool
                                .output
                                .as_ref()
                                .is_some_and(|o| o.to_string().to_lowercase().contains(&q))
                    }
                    muqun_gateway::agents::AgentPart::Diff { file, diff } => {
                        file.to_lowercase().contains(&q) || diff.to_lowercase().contains(&q)
                    }
                    muqun_gateway::agents::AgentPart::Status { text } => {
                        text.to_lowercase().contains(&q)
                    }
                    muqun_gateway::agents::AgentPart::Synthetic { text, description } => {
                        text.to_lowercase().contains(&q)
                            || description
                                .as_ref()
                                .is_some_and(|d| d.to_lowercase().contains(&q))
                    }
                    muqun_gateway::agents::AgentPart::System { text, description } => {
                        text.to_lowercase().contains(&q)
                            || description
                                .as_ref()
                                .is_some_and(|d| d.to_lowercase().contains(&q))
                    }
                    _ => false,
                };
                if matches {
                    self.timeline_search.matches.push(item.id.clone());
                }
            }
        }
    }

    /// Jump to current active timeline search match.
    pub fn timeline_search_jump_to_current(&mut self, tx: &mut rune::Tx) {
        let Some(asid) = self.selected.clone() else {
            return;
        };
        if self.timeline_search.matches.is_empty() {
            return;
        }
        let idx = self.timeline_search.active_index % self.timeline_search.matches.len();
        let key = self.timeline_search.matches[idx].clone();
        tx.scroll_center_in(format!("feed-{asid}"), key.clone());
        if let Some(tl) = self.timelines.get_mut(&asid) {
            tl.selected = Some(key.clone());
            tl.flash = Some((key, std::time::Instant::now()));
            tl.rev += 1;
        }
        tx.wake_after(std::time::Duration::from_millis(800));
    }

    /// Jump to next timeline search match.
    pub fn timeline_search_next(&mut self, tx: &mut rune::Tx) {
        if self.timeline_search.matches.is_empty() {
            return;
        }
        self.timeline_search.active_index =
            (self.timeline_search.active_index + 1) % self.timeline_search.matches.len();
        self.timeline_search_jump_to_current(tx);
    }

    /// Jump to previous timeline search match.
    pub fn timeline_search_prev(&mut self, tx: &mut rune::Tx) {
        if self.timeline_search.matches.is_empty() {
            return;
        }
        if self.timeline_search.active_index == 0 {
            self.timeline_search.active_index = self.timeline_search.matches.len() - 1;
        } else {
            self.timeline_search.active_index -= 1;
        }
        self.timeline_search_jump_to_current(tx);
    }

    pub fn go(&mut self, mode: Mode) {
        self.mode = mode;
        match mode {
            Mode::Tasks => self.send(Command::RefreshTasks),
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

    /// Keep the right-hand panes within the window beside the sidebar
    /// (`panes.rs`). Whether a width changed.
    fn fit_panes(&mut self) -> bool {
        let window = self.fits.window;
        if window <= 0.0 {
            return false;
        }
        let max = panes::right_max(window, self.sidebar.shown_width());
        let f = &mut self.fits;
        let term = f.term.apply(&mut self.term.pane, max);
        let inspect = f.inspect.apply(&mut self.inspect_pane, max);
        let right = f.right.apply(&mut self.right, max);
        term | inspect | right
    }

    /// Save the panes' widths when they changed (a drag, a double-click
    /// reset). Waits for a drag in progress to end.
    pub fn persist_layout(&mut self) {
        if self.sidebar.is_resizing() || self.right.is_resizing() || self.term.pane.is_resizing() {
            LAYOUT_DIRTY.store(true, Ordering::Release);
            return;
        }
        let sidebar = self.sidebar.width();
        let right = self.fits.right.chosen(&self.right);
        if (sidebar - self.cfg.ui.sidebar_width).abs() >= 1.0 {
            self.edit(ConfigEdit::SidebarWidth(sidebar));
        }
        if (right - self.cfg.ui.right_width).abs() >= 1.0 {
            self.edit(ConfigEdit::RightWidth(right));
        }
        let terminal = self.fits.term.chosen(&self.term.pane);
        if (terminal - self.cfg.ui.terminal_width).abs() >= 1.0 {
            self.edit(ConfigEdit::TerminalWidth(terminal));
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
        if self.term.open {
            // One panel on the right at a time: the inspector takes it back.
            self.close_terminal();
            self.right_open = false;
        }
        self.right_open = !self.right_open;
        self.right.set_open(self.right_open);
        if self.right_open {
            self.refresh_inspector();
        }
    }

    pub fn open_git_inspector(&mut self) {
        if self.mode != Mode::Session && self.selected.is_some() {
            self.go(Mode::Session);
        }
        if self.term.open {
            self.close_terminal();
        }
        self.inspector_tab = InspectorTab::Git;
        self.right_open = true;
        self.right.set_open(true);
        self.refresh_inspector();
    }

    pub fn toggle_git_inspector(&mut self) {
        if self.mode != Mode::Session && self.selected.is_some() {
            self.go(Mode::Session);
        }
        if self.right_open && self.inspector_tab == InspectorTab::Git && !self.term.open {
            self.right_open = false;
            self.right.set_open(false);
        } else {
            self.open_git_inspector();
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
            diff_active_file: std::mem::take(&mut self.diff_active_file),
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
        self.diff_active_file = slot.diff_active_file;
        self.revert_staged = slot.revert_staged;
        self.catalog = slot.catalog;
        self.descendants = slot.descendants;
        self.saved_permissions = slot.saved_permissions;
    }

    /// Give tile `i` the keyboard: its session becomes the one the top-level
    /// fields describe, the old focused tile keeps its state in its slot.
    pub fn focus_tile(&mut self, i: usize) {
        if i >= self.tiles.len() {
            self.mode = Mode::Session;
            return;
        }
        if i == self.focus {
            self.mode = Mode::Session;
            if self.selected != self.tiles[i].asid {
                self.selected = self.tiles[i].asid.clone();
                self.sync_watch();
                if self.catalog.is_none() {
                    if let Some(info) = self.current().cloned() {
                        self.session_opened(&info);
                    }
                }
            }
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

    /// Tell the core which sessions are on screen in split tiles, and who is focused.
    fn sync_watch(&self) {
        let watched: Vec<AgentSessionId> = self
            .tiles
            .iter()
            .filter_map(|t| t.asid.clone().map(AgentSessionId))
            .collect();
        self.send(Command::WatchSessions(watched));
        if let Some(ref asid) = self.selected {
            self.send(Command::FocusSession(AgentSessionId(asid.clone())));
        }
    }

    /// Open `asid` beside the current session(s): it fills an empty pane,
    /// or splits the focused pane (Hyprland's dwindle), or at the most
    /// panes takes the focused pane's place. The pane it lands in takes
    /// focus; a session already in a pane just takes focus.
    pub fn open_in_tile(&mut self, asid: &str) {
        if let Some(i) = self.tile_of(asid) {
            self.focus_tile(i);
            return;
        }
        self.right_open = false;
        self.right.set_open(false);
        if self.tiles.is_empty() {
            let Some(current) = self.selected.clone() else {
                self.open_here(asid);
                return;
            };
            self.tiles.push(Tile {
                asid: Some(current),
                slot: SessionSlot::default(),
            });
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }
        let at = match self.tiles.iter().position(|t| t.asid.is_none()) {
            Some(empty) => empty,
            None if self.tiles.len() < MAX_TILES => self.add_tile(),
            None => {
                self.open_here(asid);
                return;
            }
        };
        self.tiles[at].asid = Some(asid.to_string());
        self.tiles[at].slot = SessionSlot {
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
        self.mode = Mode::Session;
        self.send(Command::OpenTile(AgentSessionId(asid.to_string())));
        self.focus_tile(at);
        self.sync_watch();
        self.persist_tiles();
    }

    /// An empty tile splitting the focused one; its index.
    fn add_tile(&mut self) -> usize {
        let mut tree = self.tile_layout();
        let new = self.tiles.len();
        dwindle::insert(&mut tree, self.focus, new, self.tile_grid);
        self.tile_tree = tree;
        self.tiles.push(Tile {
            asid: None,
            slot: SessionSlot::default(),
        });
        new
    }

    /// How the tiles sit: the kept tree, or (when it no longer matches the
    /// tiles, as a hand-set demo's) the dwindle spiral of as many.
    pub fn tile_layout(&self) -> crate::config::TileNode {
        let n = self.tiles.len().max(1);
        if self.tile_tree.fits(n) {
            self.tile_tree.clone()
        } else {
            dwindle::build(n, self.tile_grid)
        }
    }

    /// Set the share of the split at `path` (dragging the gap in it).
    pub fn set_tile_ratio(&mut self, path: &[bool], ratio: f32) {
        let mut tree = self.tile_layout();
        if let Some(crate::config::TileNode::Split { ratio: r, .. }) = tree.at_mut(path) {
            *r = ratio.clamp(dwindle::MIN_RATIO, 1.0 - dwindle::MIN_RATIO);
        }
        self.tile_tree = tree;
    }

    /// Add an empty pane splitting the focused one, offering a session to
    /// pick (the Split action), and focus it.
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
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }
        let new = self.add_tile();
        self.mode = Mode::Session;
        self.focus_tile(new);
    }

    /// Leave split view: keep the focused pane's session, close the others.
    pub fn unsplit(&mut self) {
        let mut j = self.tiles.len();
        while j > 0 {
            j -= 1;
            if j != self.focus && self.tiles.len() > 1 {
                self.close_tile(j);
            }
        }
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
        self.tile_tree = dwindle::remove(self.tile_layout_of(self.tiles.len() + 1), i)
            .unwrap_or(crate::config::TileNode::Leaf(0));
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
            self.tile_tree = crate::config::TileNode::Leaf(0);
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

    /// The kept tree if it tiles `n` panes, else the spiral of `n`.
    fn tile_layout_of(&self, n: usize) -> crate::config::TileNode {
        if self.tile_tree.fits(n) {
            self.tile_tree.clone()
        } else {
            dwindle::build(n, self.tile_grid)
        }
    }

    /// Swap tiles `i` and `j`: their sessions trade places (focus follows
    /// its session).
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

    /// Remember the split for the next launch: its sessions and their
    /// tiling, empty panes left out.
    pub fn persist_tiles(&mut self) {
        let asids: Vec<String> = self.tiles.iter().filter_map(|t| t.asid.clone()).collect();
        let mut layout = (self.tiles.len() >= 2).then(|| self.tile_layout());
        for (i, t) in self.tiles.iter().enumerate().rev() {
            if t.asid.is_none() {
                layout = layout.and_then(|l| dwindle::remove(l, i));
            }
        }
        let layout = layout.filter(|l| asids.len() >= 2 && l.fits(asids.len()));
        if asids != self.cfg.ui.tiles || layout != self.cfg.ui.tile_layout {
            self.edit(ConfigEdit::Tiles { asids, layout });
        }
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
        // The saved tiling, when every saved pane came back.
        if let Some(layout) = self.cfg.ui.tile_layout.clone() {
            if saved.len() == self.cfg.ui.tiles.len() && layout.fits(self.tiles.len()) {
                self.tile_tree = layout;
            }
        }
        self.focus_tile(0);
        self.persist_tiles();
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
                Update::ComputerPairingRequested {
                    server_id: _,
                    request_id,
                    expires_in_ms: _,
                    transport_required,
                } => {
                    self.pairing_busy = false;
                    self.pairing_request_id = Some(request_id);
                    self.pairing_transport_required = transport_required;
                    self.pairing_error = None;
                    self.pairing_code_input.focused = true;
                    self.toast(
                        copy::tr(
                            self.cfg.general.language,
                            "Pairing code requested. Check the remote computer.",
                        ),
                        None,
                    );
                }
                Update::ComputerPairingOutcome {
                    server_id,
                    token,
                    error,
                } => {
                    self.pairing_busy = false;
                    if let Some(tok) = token {
                        self.close_pairing();
                        let comp = self.cfg.computers.entry(server_id.clone()).or_default();
                        comp.token = Some(tok);
                        let name = self.computer_name(&server_id);
                        self.toast(
                            format!(
                                "{}: {}",
                                name,
                                copy::tr(self.cfg.general.language, "Connected successfully")
                            ),
                            None,
                        );
                        self.switch_computer(server_id);
                    } else if let Some(err) = error {
                        self.pairing_error = Some(err);
                    }
                }
                Update::AgentSessions { agent, sessions } => {
                    self.listed_agents.insert(agent.clone());
                    let curr = self.current_computer().to_string();
                    if curr != "all" {
                        self.sessions.retain(|s| s.agent_id != agent);
                        self.merge_sessions(sessions);
                        if self.error.as_ref().is_some_and(|e| e.unreachable) {
                            self.error = None;
                        }
                        self.restore_tiles();
                    } else {
                        let local_data = self.computer_data.entry("local".to_string()).or_default();
                        local_data.sessions.retain(|s| s.agent_id != agent);
                        for s in sessions {
                            if let Some(existing) =
                                local_data.sessions.iter_mut().find(|x| x.asid == s.asid)
                            {
                                *existing = s;
                            } else {
                                local_data.sessions.push(s);
                            }
                        }
                        let mut all_sess = Vec::new();
                        for cdata in self.computer_data.values() {
                            for sess in &cdata.sessions {
                                if !all_sess
                                    .iter()
                                    .any(|x: &AgentSessionInfo| x.asid == sess.asid)
                                {
                                    all_sess.push(sess.clone());
                                }
                            }
                        }
                        all_sess.sort_by_key(|a| std::cmp::Reverse(a.updated_ms));
                        self.sessions = all_sess;
                    }
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
                    if prefix == self.folder_query.text.trim() {
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
                Update::MediaReady => {
                    // Placeholders become pictures: rows measure again.
                    for tl in self.timelines.values_mut() {
                        tl.rev += 1;
                    }
                }
                Update::Tasks { rows, default_cwd } => {
                    self.tasks = rows;
                    self.task_cwd = default_cwd;
                    tasks::listed(self);
                }
                Update::TasksFailed(why) => self.tasks_view.error = Some(why),
                Update::TaskHistory { id, steps } => {
                    self.tasks_view.history.insert(id, steps);
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
                Update::HubLink(down) => self.hub_down = down,
                Update::Search {
                    generation,
                    outcome,
                } => self.on_search(generation, outcome),
                Update::Attention(attention) => self.on_attention(&attention),
                Update::Voice(event) => self.on_voice(event),
                Update::UpdateState(state) => self.update_state = state,
            }
        }
        if LAYOUT_DIRTY.swap(false, Ordering::AcqRel) {
            self.persist_layout();
        }
        self.note_busy();
        inbox::note_waiting(self);
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
                let done = matches!(&job, Job::Done { .. });
                self.theme_jobs.insert(id.clone(), job);
                if done && !id.starts_with("import:") {
                    let comp = self.current_computer().to_string();
                    let scheme = self.cfg.appearance.scheme;
                    self.edit(ConfigEdit::ComputerTheme {
                        computer: comp,
                        theme_id: id.clone(),
                    });
                    self.send(Command::ThemeApply(id, scheme));
                }
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
                self.echo_rows(&asid);
                self.waiting_perms
                    .insert(asid.clone(), snapshot.permissions);
                self.waiting_forms.insert(asid.clone(), snapshot.forms);
                let queue = InboxItem::list(&snapshot.inbox);
                self.echo_inbox(&asid, &queue);
                self.tiles[i].slot.queue = queue;
                return;
            }
        }
        // A session started from an empty tile lands in it.
        if let Some(i) = self.pending_tile.take() {
            if self.tiles.get(i).is_some_and(|t| t.asid.is_none()) {
                self.tiles[i].asid = Some(asid.clone());
                self.focus_tile(i);
                self.sync_watch();
            }
        }
        // A session started while split view is up joins it in a new pane
        // (at the most panes, it takes the focused one's place below).
        if self.in_split() && self.tile_of(&asid).is_none() && self.tiles.len() < MAX_TILES {
            let at = self.add_tile();
            self.focus_tile(at);
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
        self.echo_rows(&asid);
        if fresh {
            self.sync_watch();
            self.retain_timelines();
        }
        self.waiting_perms
            .insert(asid.clone(), snapshot.permissions);
        self.waiting_forms.insert(asid.clone(), snapshot.forms);
        let queue = InboxItem::list(&snapshot.inbox);
        self.echo_inbox(&asid, &queue);
        self.queue = queue;
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
                self.echo_rows(&asid.0);
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
                let items = InboxItem::list(&items);
                self.echo_inbox(&asid.0, &items);
                if current(self, &asid) {
                    self.queue = items;
                } else if let Some(slot) = self.parked_slot(&asid.0) {
                    slot.queue = items;
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
                    if let Some(diff) = &self.diff {
                        if let Some(idx) = diff.file_index(&path) {
                            self.diff_active_file = idx;
                        }
                    }
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
            UiAction::ImagePasted(png) => {
                let token = self.next_token;
                composer::stage_png(self, png);
                let draft = if self.mode == Mode::Session {
                    &mut self.draft
                } else {
                    &mut self.start_draft
                };
                composer::insert_image_pill(draft, token);
            }
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
                // An echoed prompt left the draft when it was sent; its
                // bubble says how the send went.
                if self.echo_submitted(token, ok) {
                    if self.pending_submit.is_some_and(|(p, _)| p == token) {
                        self.pending_submit = None;
                    }
                    self.status = if ok { "Sent" } else { "Send failed" }.into();
                    return;
                }
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
                self.echo_inbox(&asid, &items);
                if self.selected.as_deref() == Some(asid.as_str()) {
                    self.queue = items;
                } else if let Some(slot) = self.parked_slot(&asid) {
                    slot.queue = items;
                }
            }
            ComposerUpdate::Branch { cwd, info } => {
                self.branches.insert(cwd, info);
            }
            ComposerUpdate::Peers { peers } => {
                for p in &peers {
                    if p.server_id != "self" && p.server_id != "local" {
                        let entry = self.cfg.computers.entry(p.server_id.clone()).or_default();
                        if !p.name.is_empty() {
                            entry.name = p.name.clone();
                        }
                        if let Some(url) = &p.url {
                            entry.url = Some(url.clone());
                        }
                    }
                }
                self.peers = peers;
            }
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

/// A page that owns the whole card, with no toolbar over it (Inbox, Tasks):
/// the stage rises under the card's empty toolbar row so its panes start at
/// the card's top edge.
pub fn card_stage(key: &str, panes: El) -> El {
    let lift = rune_kit::shell::TOOLBAR_HEIGHT;
    let mut stage = rune::col().key(format!("{key}-stage")).w_full();
    // As tall as the whole card (not shrunk to the room under the row).
    stage = match rune::memo::layout_with("main-pane", |r| r[3]) {
        Some(h) => stage.h(h).min_h(h).offset(0.0, -lift),
        None => stage.h_full(),
    };
    // The shell sizes a page to the room under the toolbar row; the stage
    // inside keeps its own height past that.
    rune::col()
        .key(format!("{key}-page"))
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(stage.child(panes.w_full().h_full().min_h(0.0)))
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

    /// The selected text of the focused field, when it has a selection.
    fn focused_selection(&self) -> Option<String> {
        [
            &self.draft,
            &self.start_draft,
            &self.search,
            &self.folder_query,
            &self.task_goal,
            &self.theme_url,
            &self.theme_query,
            &self.cwd_input,
            &self.rename_edit,
            &self.project_edit,
        ]
        .into_iter()
        .chain(self.form_inputs.values())
        .chain(self.agent_inputs.values())
        .find(|e| e.focused)
        .map(|e| e.selected().to_string())
        .filter(|t| !t.is_empty())
    }

    /// Start the clock of every session that began working, stop the rest.
    fn note_busy(&mut self) {
        let now = now_ms();
        let working: HashSet<String> = self
            .sessions
            .iter()
            .filter(|i| {
                matches!(
                    i.status,
                    AgentSessionStatus::Busy | AgentSessionStatus::Retry
                )
            })
            .map(|i| i.asid.0.clone())
            .collect();
        self.busy_since.retain(|asid, _| working.contains(asid));
        for asid in working {
            self.busy_since.entry(asid).or_insert(now);
        }
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
    if updated_ms == 0 {
        return String::new();
    }
    let secs = now.saturating_sub(updated_ms) / 1000;
    match secs {
        0..=59 => tr(language, "now"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A small status pill: a dot (or spinner) and a word, in the state's colour.
/// How much room a column has, read from its last layout: chrome drops
/// its secondary pieces as a right-hand panel squeezes the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fit {
    /// Under ~400 pt: one thing per line, icons for words.
    Tight,
    /// Under ~640 pt: secondary pieces fold away.
    Narrow,
    Wide,
}

impl Fit {
    pub fn of_width(width: f32) -> Fit {
        if width < 400.0 {
            Fit::Tight
        } else if width < 640.0 {
            Fit::Narrow
        } else {
            Fit::Wide
        }
    }

    /// The fit of the element keyed `key`, as it was last laid out (wide
    /// before its first layout).
    pub fn of(key: &str) -> Fit {
        rune::memo::layout_with(key, |r| Fit::of_width(r[2])).unwrap_or(Fit::Wide)
    }
}

pub fn activity_pill(
    k: &Kit,
    look: &Look,
    activity: Activity,
    language: crate::config::Language,
) -> El {
    activity_mark(k, look, activity, language, true)
}

/// The activity pill, or with `word` false only its mark (a narrow
/// header), the word kept as its label and tooltip.
pub fn activity_mark(
    k: &Kit,
    look: &Look,
    activity: Activity,
    language: crate::config::Language,
    word_shown: bool,
) -> El {
    let (color, word) = match activity {
        Activity::Working => (k.c.text_accent, tr(language, "Working")),
        Activity::NeedsYou => (k.c.warning.icon, tr(language, "Needs you")),
        Activity::Failed => (k.c.danger.icon, tr(language, "Failed")),
        Activity::Idle => (k.c.text_faint, tr(language, "Idle")),
    };
    let mark = match activity {
        Activity::Working => rune_kit::Spinner::new().size(10.0).color(color).build(k),
        Activity::NeedsYou => look.pulse(rune::el().size(7.0, 7.0).radius(3.5).fill(color)),
        _ => rune::el().size(7.0, 7.0).radius(3.5).fill(color),
    };
    let pill = rune::row()
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
        .help(word.clone())
        .child(mark.a11y_hidden());
    if !word_shown {
        return pill.px(6.0);
    }
    pill.child(
        rune::text(word)
            .font(k.ty.small_medium)
            .color(color)
            .a11y_hidden(),
    )
}

use copy::tr;

pub fn on_key(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if keymap::dispatch(s, ev, tx) {
        return true;
    }
    // Shift+Insert pastes and Ctrl+Insert copies in every field, as Ctrl+V
    // and Ctrl+C do: dictation and clipboard tools paste with Shift+Insert.
    if ev.key == rune::Key::Insert && !ev.mods.alt {
        if ev.mods.shift && !ev.mods.ctrl {
            tx.request_paste();
            return true;
        }
        if ev.mods.ctrl && !ev.mods.shift {
            if let Some(text) = s.focused_selection() {
                tx.copy(text);
                return true;
            }
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
    parts::pane::row_menu(s, &k, &mut layer);
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
    viewer::mount(s, &k, cx.env().scale, &mut layer);
    palette::mount(s, &k, &mut layer);
    shortcuts::mount(s, &k, &mut layer);
    pair_computer::mount(s, &k, &mut layer);
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
