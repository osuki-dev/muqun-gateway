//! Muqun Desktop UI: rune-kit chrome and controls over the core's [`Update`]s.
//!
//! `UiState` is the main-thread model. [`apply_updates`](UiState::apply_updates) folds
//! every core update into it, each page sends [`Command`]s back, and [`view`] lays the
//! shell out from it.

mod agent_marks;
mod agents;
pub(crate) mod art;
mod composer;
mod composer_row;
pub mod copy;
mod desktop_update;
mod download_panel;
mod dwindle;
mod echo;
pub mod fx;
mod gateway_setup;
mod home;
pub mod i18n;
pub(crate) mod icons;
pub(crate) mod inbox;
pub(crate) mod inspector;
pub mod keymap;
mod labels;
mod model_picker;
mod notifications;
pub(crate) mod orchestration;
mod package_activity;
pub mod pair_computer;
pub(crate) mod palette;
mod panes;
pub(crate) mod parts;
mod running;
pub mod scenes;
mod search;
mod session_lineage;
pub(crate) mod session_tree;
mod sessions;
pub(crate) mod settings;
mod shell;
mod shortcuts;
mod splash;
pub(crate) mod tasks;
pub(crate) mod terminal_panel;
mod tiles;
mod timeline;
mod tray;
pub(crate) mod viewer;
mod voice;
mod welcome;
mod windows;

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
use rune_kit::Kit;

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
    Mao,
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

/// Severity level for bottom-right notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToastLevel {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

/// A transient notice (theme service answers, failed sends, errors).
#[derive(Debug, Clone)]
pub struct ToastItem {
    pub id: u64,
    pub level: ToastLevel,
    pub title: String,
    pub detail: Option<String>,
    pub created_at: std::time::Instant,
    pub target: Option<String>,
}

const MAX_TOASTS: usize = 5;

/// How many gallery themes the Themes tab lists before "Show more".
pub(crate) const GALLERY_PAGE: usize = 36;

/// A pane's grip or toggle was touched: the width may have changed and
/// [`UiState::persist_layout`] should look (set from the pane access
/// functions, read by the inbox pump).
static LAYOUT_DIRTY: AtomicBool = AtomicBool::new(false);
static HAS_TOASTS: AtomicBool = AtomicBool::new(false);

fn sidebar_pane(s: &mut UiState) -> &mut PaneState {
    LAYOUT_DIRTY.store(true, Ordering::Release);
    &mut s.sidebar
}

fn inspect_pane(s: &mut UiState) -> &mut PaneState {
    &mut s.inspect_pane
}

pub use parts::open_markdown_link;
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
    /// The Git inspector's inline branch switcher dropdown.
    InspectorBranch,
    /// The session composer's permission mode menu, and its overflow
    /// (what a narrow composer folds away).
    Mode,
    ComposerMore,
    ComposerDelivery,
    /// The start page's model, mode and branch menus.
    StartModel,
    StartEffort,
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
    /// The sidebar inventory scope. Search remains global.
    SidebarScope,
    PluginSelect(String),
    PluginTextSelection(String),
    WindowSurface(String),
    DockActions,
    ViewerImageActions,
    /// The tasks page's computer filter picker.
    TasksComputerPicker,
    /// MAO reviewer agent / model / effort / rounds menu, keyed by surface anchor.
    MaoReviewer(orchestration::MaoMenuAnchor),
    /// Settings → Agents MAO candidate pool add-entry menu.
    MaoPoolAdd,
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
    catalog_request: u64,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
}

/// What kind of window or view is hosted in a split tile.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TileContent {
    /// Waiting for content to be picked into it.
    #[default]
    Empty,
    /// An AI agent chat session.
    Session(String),
    /// An embedded interactive terminal.
    Terminal,
    /// A git diff / working tree comparison view.
    #[allow(dead_code)]
    Diff(String),
    /// A generic custom or extension window.
    #[allow(dead_code)]
    Custom {
        id: String,
        kind: String,
        title: String,
    },
}

impl TileContent {
    pub fn is_empty(&self) -> bool {
        matches!(self, TileContent::Empty)
    }

    #[allow(dead_code)]
    pub fn is_session(&self) -> bool {
        matches!(self, TileContent::Session(_))
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, TileContent::Terminal)
    }

    #[allow(dead_code)]
    pub fn asid(&self) -> Option<&str> {
        match self {
            TileContent::Session(ref id) => Some(id),
            _ => None,
        }
    }

    pub fn to_persist_key(&self) -> Option<String> {
        match self {
            TileContent::Empty => None,
            TileContent::Session(id) => Some(id.clone()),
            TileContent::Terminal => Some("terminal".to_string()),
            TileContent::Diff(path) => Some(format!("diff:{path}")),
            TileContent::Custom { kind, .. } if kind == parts::detail::ID => None,
            TileContent::Custom { id, kind, .. } => Some(format!("custom:{kind}:{id}")),
        }
    }

    pub fn from_persist_key(key: &str) -> Self {
        if key == "terminal" {
            TileContent::Terminal
        } else if let Some(path) = key.strip_prefix("diff:") {
            TileContent::Diff(path.to_string())
        } else if let Some(rest) = key.strip_prefix("session:") {
            TileContent::Session(rest.to_string())
        } else if let Some(rest) = key.strip_prefix("custom:") {
            let mut parts = rest.splitn(2, ':');
            let kind = parts.next().unwrap_or("").to_string();
            let id = parts.next().unwrap_or("").to_string();
            TileContent::Custom {
                id,
                kind,
                title: "Window".to_string(),
            }
        } else {
            TileContent::Session(key.to_string())
        }
    }
}

/// One pane of the split session view.
#[derive(Default)]
pub struct Tile {
    pub content: TileContent,
    pub asid: Option<String>,
    pub term_tab: Option<usize>,
    pub slot: SessionSlot,
    /// Whether this tile is popped out as a pinned floating overlay window (Omarchy `Super+O`).
    pub popped: bool,
    /// Custom floating geometry `[x, y, w, h]` when floating or popped out.
    pub floating_rect: Option<[f32; 4]>,
}

impl Tile {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn session(asid: impl Into<String>) -> Self {
        let asid = asid.into();
        Self {
            content: TileContent::Session(asid.clone()),
            asid: Some(asid),
            ..Self::default()
        }
    }

    pub fn terminal() -> Self {
        Self {
            content: TileContent::Terminal,
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    #[allow(dead_code)]
    pub fn is_session(&self) -> bool {
        self.content.is_session()
    }

    pub fn is_terminal(&self) -> bool {
        self.content.is_terminal()
    }

    pub fn set_session(&mut self, asid: impl Into<String>) {
        let asid = asid.into();
        self.asid = Some(asid.clone());
        self.term_tab = None;
        self.content = TileContent::Session(asid);
    }

    pub fn set_terminal(&mut self) {
        self.asid = None;
        self.content = TileContent::Terminal;
    }

    pub fn set_terminal_tab(&mut self, tab: usize) {
        self.asid = None;
        self.term_tab = Some(tab);
        self.content = TileContent::Terminal;
    }
}

/// How many sessions/windows may be open side by side.
pub const MAX_TILES: usize = 8;

static INITIAL_CONFIG: OnceLock<DesktopConfig> = OnceLock::new();

/// Hand the loaded config to the UI before the window opens.
pub fn seed_config(config: DesktopConfig) {
    keymap::sync_active_keymap(&config.keymap);
    let _ = INITIAL_CONFIG.set(config);
}

/// Cached per-computer workspace state so switching machines is instant.
#[derive(Default)]
pub struct ComputerData {
    focused_slot: Option<SessionSlot>,
    start_draft: Option<TextEdit>,
    start_folder: Option<(TextEdit, bool)>,
    form_inputs: HashMap<String, TextEdit>,
    pub session_open_errors: HashMap<String, String>,
    pub pending_receipts: Vec<Update>,
    pub pending_submit: Option<(u64, Slot)>,
    pub pending_start: Option<(String, u64)>,
    pub echoes: HashMap<String, Vec<echo::Echo>>,
    pub tile_tree: Option<crate::config::TileNode>,
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
    pub session_open_errors: HashMap<String, String>,
    computer_generations: HashMap<String, u64>,
    pub commands: CommandSender,
    /// A mirror of the owner's config: what the pages show, kept in step with
    /// every [`ConfigEdit`] sent to the core.
    pub cfg: DesktopConfig,
    pub dark: bool,
    pub system_dark: std::cell::Cell<Option<bool>>,
    pub system_reduced_motion: std::cell::Cell<bool>,
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
    /// Full-size image above its originating session or plugin.
    pub zoom: Option<Box<viewer::Viewer>>,
    /// A subagent's session shown read-only over its parent.
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
    /// Sessions currently executing a context compaction.
    pub compacting_sessions: HashSet<String>,
    /// Account rate windows by agent, once asked for (`None`: it has none).
    pub account_limits: HashMap<String, Option<serde_json::Value>>,
    pub agent_details: std::collections::HashSet<String>,
    pub status: String,
    pub hub_label: String,
    pub error: Option<UiError>,
    pub error_at: Option<std::time::Instant>,

    pub draft: TextEdit,
    pub start_draft: TextEdit,
    pub start_agent: String,
    pub start_cwd: TextEdit,
    start_cwd_explicit: bool,
    pub start_worktree: bool,
    /// The start page's model and mode picks; `None` leaves the agent's own.
    pub start_model: Option<ModelRef>,
    pub start_mode: Option<String>,
    pub start_catalog: Option<Arc<CatalogView>>,
    start_catalog_request: u64,
    start_catalog_failed: bool,
    pub completions: Vec<DirectoryEntry>,
    pub staged: Vec<StagedFile>,
    pub queue: Vec<InboxItem>,
    pub file_hits: Vec<FileHit>,
    pub catalog: Option<Arc<CatalogView>>,
    catalog_request: u64,
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
    pub deferred_attachments: HashMap<(String, Option<String>), Vec<StagedFile>>,
    pub pending_file_targets: HashMap<u64, (String, Option<String>)>,
    pub pending_image_uploads: std::collections::HashSet<u64>,
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
    pub git_commit_msg: TextEdit,

    pub tasks: Vec<TaskRow>,
    pub task_cwd: Option<String>,
    pub task_goal: TextEdit,
    pub descendants: Vec<AgentSessionInfo>,
    pub saved_permissions: Vec<SavedPermission>,
    pub shells: Vec<ShellRow>,
    pub worktrees: Vec<WorktreeRow>,
    /// Whether the Git inspector expanded beyond the initial 12 changed files.
    pub inspector_git_show_all: bool,
    /// Pending two-step confirmation key inside the Inspector (discard/remove/stop).
    pub inspector_confirm: Option<String>,
    /// Guards against stale in-flight `Update::Shells` after switching sessions/computers.
    inspector_shells_expect: bool,
    /// Guards against stale in-flight `Update::Worktrees` after switching sessions/computers.
    inspector_worktrees_expect: bool,

    pub search: TextEdit,
    pub sidebar_all_sessions: bool,
    pub sidebar_opened: HashMap<String, u64>,
    pub pairing_target: Option<String>,
    pub pairing_attempt: u64,
    pub pairing_offer: Option<crate::platform::pairing::PairingOffer>,
    pub pairing_code_input: TextEdit,
    pub pairing_token_input: TextEdit,
    pub pairing_url_input: TextEdit,
    pub pairing_manual_token_mode: bool,
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
    pub model_query: TextEdit,
    pub model_picker: model_picker::PickerState,
    pub full_projects: HashSet<String>,

    /// Pending prompts per session, from snapshots and events.
    pub waiting_perms: HashMap<String, Vec<PermissionRequest>>,
    pub waiting_forms: HashMap<String, Vec<FormRequest>>,
    /// The Inbox page's selection and when each prompt first arrived.
    pub inbox_view: inbox::InboxView,
    pub fleet_pending: HashMap<String, Vec<crate::model::fleet_attention::Pending>>,
    /// The Tasks page's selection, filters and task histories.
    pub tasks_view: tasks::TasksView,
    pub orchestration: orchestration::View,
    /// Form answers, by `form id / field key`.
    pub form_inputs: HashMap<String, TextEdit>,

    pub agent_inputs: HashMap<String, TextEdit>,
    pub agent_errors: HashMap<String, String>,
    pub mcp_hub: settings::mcp_hub::McpHubState,
    pub cwd_input: TextEdit,
    /// Themes: the library, the gallery, running installs, the link field.
    pub theme_library: Arc<Vec<ThemeEntry>>,
    pub theme_gallery: Arc<GalleryView>,
    /// Gallery card pictures keyed by theme id (`ThemeUpdate::Preview`).
    pub theme_previews: HashMap<String, PathBuf>,
    pub theme_jobs: HashMap<String, Job>,
    pub theme_url: TextEdit,
    pub plugin_source: TextEdit,
    pub plugin_query: TextEdit,
    pub plugin_category: Option<crate::plugin::PluginCategory>,
    pub plugin_fields: HashMap<(String, String), HashMap<(String, String), TextEdit>>,
    pub diagnostic_filter: settings::DiagnosticFilter,
    pub computer_unpair: settings::computers::Unpair,
    forgotten_computers: HashSet<String>,
    pub local_revoke: settings::revoke_device::State,
    pub(crate) local_invite: Option<settings::local_devices::InvitePanel>,
    pub gateway_setup: Option<crate::platform::gateway_bootstrap::Status>,
    desktop_update_panel: bool,
    restart_checkpoint_pending: bool,
    restart_on_frame: bool,
    pub desktop_update: crate::platform::update::Status,
    pub local_pairings: crate::platform::paired_devices::List,
    pub(crate) plugin_packages: package_activity::PackageActivity,
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
    /// Whether the composer prompt history picker popup is open.
    pub composer_history_open: bool,
    pub composer_queue_delivery: bool,
    pub composer_queue_expanded: bool,
    pub composer_queue_page: Option<String>,
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
    /// Global leader key timestamp when armed (`Which-Key` mode).
    pub leader_armed: Option<std::time::Instant>,
    /// Active 2nd-level semantic sub-group in `Which-Key` mode (`+session`, `+window`, etc.).
    pub leader_group: Option<keymap::LeaderGroup>,
    /// Active shortcut recording target in `Settings → Shortcuts`.
    pub recording_key: Option<keymap::RecordingTarget>,
    /// Sink for any overlay `TextInput` during its exit transition after close.
    pub closed_edit: TextEdit,
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
    /// How the tiles sit (`dwindle.rs`); [`UiState::tile_layout`] reads it.
    pub tile_tree: crate::config::TileNode,
    /// Temporarily maximized tile index (Omarchy `Super+Alt+F`).
    pub zoomed_tile: Option<usize>,
    /// Active drag-and-drop state when reordering or re-splitting tiles by their header.
    pub tile_card_drag: Option<tiles::TileCardDrag>,
    /// An empty tile waiting for the next opened session.
    pub pending_tile: Option<usize>,
    /// Pending optimistic session being created from home composer: (placeholder_asid, token).
    pub pending_start: Option<(String, u64)>,
    /// The split's size on screen, for splitting and the gaps' drags.
    pub tile_grid: [f32; 2],
    /// Active tile split gap path being dragged.
    pub tile_drag: Option<Vec<bool>>,
    /// The saved split was put back once sessions were listed.
    tiles_restored: bool,
    /// Registered desktop plugins.
    pub plugins: crate::plugin::PluginRegistry,
    /// Active plugin opened in the full-screen Page slot (`PluginSlot::Page`).
    pub active_plugin_page: Option<String>,
    pub plugin_page_origin: Option<crate::plugin::PluginSlot>,
    moving_plugin_view: Option<(String, String)>,
    /// Active plugin opened in the right-hand Drawer slot (`PluginSlot::Drawer`).
    pub active_plugin_drawer: Option<String>,
    pub(crate) dock_tabs: Vec<String>,
    hidden_dock_tab: Option<String>,
    pub(crate) browser: crate::plugin::browser::Browser,
    /// Interactive text buffer for the Quick Notes (`"quicknotes"`) scratchpad plugin.
    pub quick_notes_edit: TextEdit,
    /// Persistent or active notes per quick notes tile.
    pub quick_notes: HashMap<String, String>,
    /// Probed status of external runtimes (`bun`, `herdr`) for System & Diagnostics.
    pub runtime_statuses: Vec<muqun_gateway::platform::runtimes::RuntimeStatus>,
    /// Runtimes currently being installed or updated in the background.
    pub runtime_installing: HashSet<muqun_gateway::platform::runtimes::RuntimeKind>,
    /// Agents whose session list has arrived (split restore waits for them).
    listed_agents: HashSet<String>,
}

impl UiState {
    pub fn new(commands: CommandSender) -> Self {
        let cfg = INITIAL_CONFIG.get().cloned().unwrap_or_default();
        let dark = !matches!(cfg.appearance.scheme, Scheme::Light);
        let mut draft = TextEdit::new("").rich();
        draft.focused = true;
        let start_draft = TextEdit::new(
            cfg.general
                .drafts
                .get(crate::config::START_DRAFT)
                .cloned()
                .unwrap_or_default(),
        )
        .rich();
        let cwd_input = TextEdit::new(
            cfg.general
                .default_cwd
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        let start_agent = cfg.general.last_agent.clone().unwrap_or_default();
        let start_cwd_text = if !matches!(cfg.current_computer.as_str(), "" | "self" | "local") {
            "~".to_owned()
        } else if !start_agent.is_empty() {
            cfg.general.initial_cwd(&start_agent).display().to_string()
        } else {
            String::new()
        };
        let mut s = Self {
            commands,
            session_open_errors: HashMap::new(),
            computer_generations: HashMap::new(),
            dark,
            system_dark: Default::default(),
            system_reduced_motion: Default::default(),
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
            compacting_sessions: HashSet::new(),
            account_limits: HashMap::new(),
            agent_details: std::collections::HashSet::new(),
            status: "Starting…".into(),
            hub_label: String::new(),
            error: None,
            error_at: None,
            draft,
            start_draft,
            start_agent,
            start_cwd: TextEdit::new(start_cwd_text),
            start_cwd_explicit: false,
            start_worktree: false,
            start_model: None,
            start_mode: None,
            start_catalog: None,
            start_catalog_request: 0,
            start_catalog_failed: false,
            completions: Vec::new(),
            staged: Vec::new(),
            queue: Vec::new(),
            file_hits: Vec::new(),
            catalog: None,
            catalog_request: 0,
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
            deferred_attachments: Default::default(),
            pending_file_targets: Default::default(),
            pending_image_uploads: Default::default(),
            pending_submit: None,
            echoes: HashMap::new(),
            echo_rev: 0,
            echo_tokens: HashSet::new(),
            diff_stat: DiffStat::default(),
            diff: None,
            show_diff: false,
            diff_active_file: 0,
            revert_staged: false,
            git_commit_msg: TextEdit::new(""),
            tasks: Vec::new(),
            task_cwd: None,
            task_goal: TextEdit::new(""),
            descendants: Vec::new(),
            saved_permissions: Vec::new(),
            shells: Vec::new(),
            worktrees: Vec::new(),
            inspector_git_show_all: false,
            inspector_confirm: None,
            inspector_shells_expect: true,
            inspector_worktrees_expect: true,
            search: TextEdit::new(""),
            sidebar_all_sessions: false,
            sidebar_opened: HashMap::new(),
            pairing_target: None,
            pairing_attempt: 0,
            pairing_offer: None,
            pairing_code_input: TextEdit::new(""),
            pairing_token_input: TextEdit::new(""),
            pairing_url_input: TextEdit::new(""),
            pairing_manual_token_mode: false,
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
            model_query: TextEdit::new(""),
            model_picker: Default::default(),
            full_projects: HashSet::new(),
            waiting_perms: HashMap::new(),
            waiting_forms: HashMap::new(),
            inbox_view: Default::default(),
            fleet_pending: HashMap::new(),
            tasks_view: Default::default(),
            orchestration: Default::default(),
            form_inputs: HashMap::new(),
            agent_inputs: HashMap::new(),
            agent_errors: HashMap::new(),
            mcp_hub: Default::default(),
            cwd_input,
            theme_library: Arc::new(Vec::new()),
            theme_gallery: Arc::new(GalleryView::default()),
            theme_previews: HashMap::new(),
            theme_jobs: HashMap::new(),
            theme_url: TextEdit::new(""),
            plugin_source: TextEdit::new(""),
            plugin_query: TextEdit::new(""),
            plugin_category: None,
            plugin_fields: HashMap::new(),
            diagnostic_filter: Default::default(),
            local_invite: None,
            gateway_setup: gateway_setup::required(crate::platform::gateway_bootstrap::status()),
            desktop_update_panel: false,
            restart_checkpoint_pending: false,
            restart_on_frame: false,
            desktop_update: Default::default(),
            local_pairings: Default::default(),
            local_revoke: Default::default(),
            computer_unpair: Default::default(),
            forgotten_computers: HashSet::new(),
            plugin_packages: Default::default(),
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
            composer_history_open: false,
            composer_queue_delivery: false,
            composer_queue_expanded: false,
            composer_queue_page: None,
            timeline_search: TimelineSearch::default(),
            agents_seen: false,
            demo: false,
            storage: None,
            viewer: None,
            voice: None,
            palette: None,
            keys_sheet: None,
            leader_armed: None,
            leader_group: None,
            recording_key: None,
            closed_edit: TextEdit::new(""),
            tray_shown: None,
            voice_form: voice::VoiceForm::new(&cfg.voice),
            term: terminal_panel::TermPanel::new(cfg.ui.terminal()),
            tiles: Vec::new(),
            focus: 0,
            tile_tree: crate::config::TileNode::Leaf(0),
            zoomed_tile: None,
            tile_card_drag: None,
            pending_tile: None,
            pending_start: None,
            tile_grid: [0.0; 2],
            tile_drag: None,
            listed_agents: HashSet::new(),
            tiles_restored: false,
            plugins: crate::plugin::PluginRegistry::default_registry(),
            active_plugin_page: None,
            plugin_page_origin: None,
            moving_plugin_view: None,
            active_plugin_drawer: None,
            dock_tabs: Vec::new(),
            hidden_dock_tab: None,
            browser: Default::default(),
            quick_notes_edit: TextEdit::new(""),
            quick_notes: HashMap::new(),
            runtime_statuses: Vec::new(),
            runtime_installing: HashSet::new(),
            cfg,
        };
        keymap::sync_active_keymap(&s.cfg.keymap);
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
        self.send(Command::ProbeRuntimes);
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
        if std::mem::take(&mut self.restart_on_frame) {
            tx.quit();
            return true;
        }
        let mut plugins = false;
        for plugin in self.plugins.all() {
            plugins |= plugin.poll(self, tx);
        }
        let voice = self.poll_voice();
        let fit = self.fit_panes();
        let term = self.poll_terminal(t, tx);
        let search = self.poll_search(tx);
        let echoes = self.poll_echoes(tx);
        self.poll_local_invite(tx)
            | self.poll_splash(t, tx)
            | voice
            | fit
            | term
            | search
            | echoes
            | plugins
    }

    pub fn poll_splash(&mut self, t: f64, _: &mut Tx) -> bool {
        let Some(splash) = self.splash.as_mut() else {
            return false;
        };
        let hub_ready = !self.hub_label.is_empty();
        if !splash.tick(t, self.agents_seen, hub_ready) {
            self.splash = None;
            return true;
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
        if let ConfigEdit::PluginEnabled { plugin, .. } = &edit {
            let enabled = crate::plugin::is_enabled(plugin, &self.cfg.plugins_disabled);
            if let Some(instance) = self.plugins.get(plugin) {
                instance.enabled_changed(enabled);
            }
            if !enabled {
                self.palette = None;
                self.menu = None;
                self.close_plugin_windows(plugin);
            }
        }
        keymap::sync_active_keymap(&self.cfg.keymap);
        self.send(Command::Edit(edit));
    }

    /// Whether the global Leader key (`Which-Key` mode) is currently armed.
    /// Stays open until Esc, a completed chord, or a Leader double-tap — no auto-timeout.
    pub fn is_leader_armed(&self) -> bool {
        self.leader_armed.is_some()
    }

    /// Arm the global Leader key (`Which-Key` mode).
    pub fn arm_leader(&mut self, _tx: &mut Tx) {
        let now = std::time::Instant::now();
        self.leader_armed = Some(now);
        self.leader_group = None;
        self.term.prefix_armed = Some(now);
    }

    /// Enter a 2nd-level semantic sub-group in `Which-Key` mode.
    pub fn arm_leader_group(&mut self, group: keymap::LeaderGroup, _tx: &mut Tx) {
        let now = std::time::Instant::now();
        self.leader_armed = Some(now);
        self.leader_group = Some(group);
        self.term.prefix_armed = Some(now);
    }

    /// Disarm the global Leader key (`Which-Key` mode).
    pub fn disarm_leader(&mut self) {
        self.leader_armed = None;
        self.leader_group = None;
        self.term.prefix_armed = None;
    }

    /// Whether a terminal pane or tile currently holds keyboard focus.
    pub fn is_terminal_focused(&self) -> bool {
        if self.in_split() {
            self.tiles.get(self.focus).is_some_and(|t| t.is_terminal()) && self.term.sink.focused
        } else {
            self.term.open && self.term.sink.focused
        }
    }

    pub fn session_by_id(&self, asid: &str) -> Option<&AgentSessionInfo> {
        self.sessions.iter().find(|s| s.asid.0 == asid)
    }

    /// Whole-session forking does not imply a message cutoff is supported.
    /// Unknown/older discoveries fail closed until an adapter explicitly advertises it.
    pub(super) fn can_fork_from_message(&self, asid: &str) -> bool {
        let Some(session) = self.session_by_id(asid) else {
            return false;
        };
        if session.runtime.as_ref().is_some_and(|runtime| {
            runtime.interaction != muqun_gateway::agents::RuntimeInteraction::Native
        }) {
            return false;
        }
        self.agents
            .iter()
            .find(|agent| agent.id == session.agent_id)
            .is_some_and(|agent| {
                ["sessionFork", "sessionForkFromMessage"].iter().all(|key| {
                    agent
                        .features
                        .extra
                        .get(*key)
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                })
            })
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
        if self.cfg.current_computer.is_empty() || self.cfg.current_computer == "self" {
            "local"
        } else {
            &self.cfg.current_computer
        }
    }

    pub fn can_connect_new_local_gateway(&self) -> bool {
        self.current_computer() == "local"
            && self.selected.is_none()
            && self.tiles.is_empty()
            && self.busy_since.is_empty()
            && self.pending_submit.is_none()
            && self.pending_start.is_none()
            && self.start_draft.text.trim().is_empty()
            && !self.term.open
            && !self.cfg.general.gateway_local_only
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
            if profile.url.is_none() && profile.token.is_none() {
                continue;
            }
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
        let profile = self.cfg.computers.get(computer_id);
        if let Some(saved) = profile
            .filter(|p| p.token.is_some())
            .and_then(|p| p.url.clone())
        {
            return Some(saved);
        }
        self.peer_by_id(computer_id)
            .and_then(|p| p.url.clone())
            .or_else(|| profile.and_then(|p| p.url.clone()))
    }

    pub fn computer_of_session(&self, asid: &str) -> Option<String> {
        let curr = self.current_computer();
        if curr != "all" && self.sessions.iter().any(|s| s.asid.0 == asid) {
            return Some(curr.to_string());
        }
        for (cid, cdata) in &self.computer_data {
            if cid == "all" {
                continue;
            }
            if cdata.sessions.iter().any(|s| s.asid.0 == asid) {
                return Some(cid.clone());
            }
        }
        if self.sessions.iter().any(|s| s.asid.0 == asid) {
            return Some("local".to_string());
        }
        None
    }

    /// The computer that owns the active session (or the currently selected computer).
    pub fn active_session_computer(&self) -> String {
        if let Some(asid) = self.selected.as_deref() {
            if let Some(comp) = self.computer_of_session(asid) {
                return comp;
            }
        }
        self.current_computer().to_string()
    }

    /// Whether `computer_id` names a paired remote computer rather than this machine.
    pub fn is_remote_computer_id(computer_id: &str) -> bool {
        !matches!(computer_id, "" | "local" | "self" | "all")
    }

    /// Whether the active session (or currently selected computer) runs on a remote computer.
    pub fn is_remote_session(&self) -> bool {
        Self::is_remote_computer_id(&self.active_session_computer())
    }

    /// Build a [`crate::hub::LocalGateway`] credential for a saved remote computer.
    pub fn remote_gateway_for(&self, computer_id: &str) -> Option<crate::hub::LocalGateway> {
        if !Self::is_remote_computer_id(computer_id) {
            return None;
        }
        let profile = self.cfg.computers.get(computer_id)?;
        let token = profile
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())?;
        let url = self
            .peer_url(computer_id)
            .map(|u| u.trim().to_owned())
            .filter(|u| !u.is_empty())?;
        Some(crate::hub::LocalGateway {
            url,
            token: token.to_owned(),
            device_id: profile.device_id.clone(),
            transport_key: profile.transport_key.clone(),
        })
    }

    fn route_session(&mut self, asid: &str) {
        if let Some(computer) = self.computer_of_session(asid) {
            if computer != self.current_computer() {
                self.switch_computer(computer);
            }
        }
    }

    pub fn retry_open_session(&mut self, asid: &str) {
        self.route_session(asid);
        self.session_open_errors.remove(asid);
        self.send(Command::OpenSession(AgentSessionId(asid.to_owned())));
    }

    fn rebuild_all_sessions(&mut self) {
        self.sessions = self
            .computer_data
            .iter()
            .filter(|(computer, _)| computer.as_str() != "all")
            .flat_map(|(_, data)| data.sessions.iter().cloned())
            .collect();
        self.sessions
            .sort_by_key(|session| std::cmp::Reverse(session.updated_ms));
        self.sessions.dedup_by(|a, b| a.asid == b.asid);
    }

    pub fn open_manual_pairing(&mut self) {
        self.open_pairing(String::new());
        self.pairing_url_input.focused = true;
        self.pairing_code_input.focused = false;
    }

    fn begin_pairing(&mut self, address: &str) -> Option<(u64, String)> {
        if self.pairing_busy {
            return None;
        }
        match crate::platform::pairing::gateway_url(address) {
            Ok(url) => {
                self.pairing_attempt = self.pairing_attempt.wrapping_add(1);
                self.pairing_busy = true;
                self.pairing_error = None;
                Some((self.pairing_attempt, url))
            }
            Err(e) => {
                self.pairing_error = Some(e.to_string());
                None
            }
        }
    }

    pub fn open_pairing(&mut self, target_id: String) {
        let default_url = self.peer_url(&target_id).unwrap_or_default();
        self.pairing_target = Some(target_id);
        self.pairing_offer = None;
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
        self.pairing_offer = None;
        self.pairing_busy = false;
        self.pairing_error = None;
        self.pairing_manual_token_mode = false;
        self.pairing_code_input.clear();
        self.pairing_code_input.focused = true;
        self.pairing_token_input.clear();
    }

    fn start_default_directory(&self, agent: &str) -> PathBuf {
        if self.current_computer() == "local" {
            self.cfg.general.initial_cwd(agent)
        } else {
            PathBuf::from("~")
        }
    }

    pub fn switch_computer(&mut self, id: String) {
        let id = if id == "self" || id.is_empty() {
            "local".to_string()
        } else {
            id
        };
        let prev = self.current_computer().to_string();
        let mut pending_receipts = Vec::new();
        if prev != id {
            self.waiting_perms.clear();
            self.waiting_forms.clear();
            self.reset_inspector_session_state();
            self.persist_draft(Slot::Session);
            let slot = self.take_slot();
            let default_cwd = if id == "local" {
                self.cfg.general.initial_cwd(&self.start_agent)
            } else {
                PathBuf::from("~")
            };
            self.completions.clear();
            self.start_catalog = None;
            self.folder_query = TextEdit::new("");
            let data = ComputerData {
                form_inputs: std::mem::take(&mut self.form_inputs),
                focused_slot: Some(slot),
                start_folder: Some((
                    std::mem::replace(
                        &mut self.start_cwd,
                        TextEdit::new(default_cwd.display().to_string()),
                    ),
                    std::mem::take(&mut self.start_cwd_explicit),
                )),
                start_draft: Some(std::mem::replace(&mut self.start_draft, TextEdit::new(""))),
                session_open_errors: std::mem::take(&mut self.session_open_errors),
                pending_receipts: Vec::new(),
                pending_submit: self.pending_submit.take(),
                pending_start: self.pending_start.take(),
                echoes: std::mem::take(&mut self.echoes),
                tile_tree: Some(self.tile_tree.clone()),
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
            if prev != "all" {
                self.computer_data.insert(prev, data);
            }

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
                if let Some((cwd, explicit)) = restored.start_folder {
                    self.start_cwd = cwd;
                    self.start_cwd_explicit = explicit;
                }
                self.form_inputs = restored.form_inputs;
                self.start_draft = restored.start_draft.unwrap_or_else(|| TextEdit::new(""));
                self.session_open_errors = restored.session_open_errors;
                self.pending_submit = restored.pending_submit;
                self.pending_start = restored.pending_start;
                self.echoes = restored.echoes;
                self.tile_tree = restored
                    .tile_tree
                    .unwrap_or(crate::config::TileNode::Leaf(0));
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
                if let Some(slot) = restored.focused_slot {
                    self.put_slot(slot);
                }
                pending_receipts = restored.pending_receipts;
            } else {
                self.tiles = vec![Tile::default()];
                self.focus = 0;
                self.selected = None;
                self.mode = Mode::Home;
            }
        }

        self.cfg.current_computer = id.clone();
        if id != "local" {
            self.start_worktree = false;
        }
        // Receipts can close an optimistic pane or issue follow-up reads.
        // Establish the backend barrier before applying any of them.
        if id != "all" {
            self.send(Command::SwitchComputer(id.clone()));
        }
        self.apply_updates(pending_receipts);
        composer::restore_attachments(self);
        self.edit(ConfigEdit::CurrentComputer(id.clone()));
        self.agent_inputs.clear();
        self.mcp_hub.reset_for_switch();
        if self.mode == Mode::Settings && self.settings_tab == SettingsTab::McpHub && id != "all" {
            self.refresh_mcp_upstreams();
        }
        self.agent_errors.clear();
        self.pairing_offer = None;
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
            self.send(Command::Refresh);
            let sessions: Vec<_> = self
                .tiles
                .iter()
                .filter_map(|tile| tile.asid.clone())
                .collect();
            for asid in sessions.into_iter().filter(|id| !id.starts_with("new-")) {
                self.session_open_errors.remove(&asid);
                self.send(Command::OpenTile(AgentSessionId(asid)));
            }
            if let Some(asid) = self.selected.clone() {
                self.send(Command::FocusSession(AgentSessionId(asid)));
            }
            self.sync_watch();
        }
    }

    pub fn toast(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.notify(ToastLevel::Info, title, detail);
    }

    pub fn toast_error(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.notify(ToastLevel::Error, title, detail);
    }

    pub fn notify(&mut self, level: ToastLevel, title: impl Into<String>, detail: Option<String>) {
        self.add_toast(level, title.into(), detail, None);
    }

    pub fn notify_diagnostic(
        &mut self,
        event: uuid::Uuid,
        level: ToastLevel,
        title: impl Into<String>,
        detail: Option<String>,
    ) {
        self.add_toast(
            level,
            title.into(),
            detail,
            Some(format!("diagnostics:{event}")),
        );
    }

    pub(crate) fn add_toast(
        &mut self,
        level: ToastLevel,
        title: String,
        detail: Option<String>,
        target: Option<String>,
    ) {
        let now = std::time::Instant::now();
        if let Some(existing) = self
            .toasts
            .iter_mut()
            .find(|t| t.target == target && t.title == title && t.detail == detail)
        {
            existing.level = level;
            existing.created_at = now;
            HAS_TOASTS.store(true, Ordering::Release);
            return;
        }
        self.toast_seq += 1;
        self.toasts.push(ToastItem {
            id: self.toast_seq,
            level,
            title,
            detail,
            created_at: now,
            target,
        });
        while self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
        HAS_TOASTS.store(true, Ordering::Release);
    }

    pub fn expire_toasts(&mut self) -> bool {
        let mut changed = false;
        if let Some(ref err) = self.error {
            if !err.unreachable
                && self
                    .error_at
                    .is_some_and(|at| at.elapsed() >= std::time::Duration::from_millis(6000))
            {
                self.error = None;
                self.error_at = None;
                changed = true;
            }
        }
        if !self.toasts.is_empty() {
            let before = self.toasts.len();
            self.toasts.retain(|t| {
                let ttl = if t.level == ToastLevel::Error {
                    5000
                } else {
                    3500
                };
                t.created_at.elapsed() < std::time::Duration::from_millis(ttl)
            });
            if self.toasts.len() != before {
                changed = true;
            }
        }
        let has_expiring_error = self.error.as_ref().is_some_and(|e| !e.unreachable);
        if self.toasts.is_empty() && !has_expiring_error {
            HAS_TOASTS.store(false, Ordering::Release);
        }
        changed
    }

    /// Clean up an optimistic `new-{token}` session placeholder when creation fails.
    pub fn cleanup_pending_start(&mut self) {
        let Some((pending_asid, _)) = self.pending_start.take() else {
            return;
        };
        self.sessions.retain(|s| s.asid.0 != pending_asid);
        if let Some(mut echoes) = self.echoes.remove(&pending_asid) {
            if self.start_draft.text.trim().is_empty() {
                if let Some(first) = echoes.drain(..).next() {
                    self.start_draft = TextEdit::new(first.text);
                }
            }
        }
        self.timelines.remove(&pending_asid);
        if let Some(i) = self.tile_of(&pending_asid) {
            if self.in_split() {
                self.close_tile(i);
            } else {
                self.tiles[i].asid = None;
                self.tiles[i].content = TileContent::Empty;
            }
        }
        if self.selected.as_deref() == Some(pending_asid.as_str()) {
            self.selected = self
                .last_selected
                .take()
                .filter(|id| self.sessions.iter().any(|s| s.asid.0 == *id));
            if self.selected.is_none() {
                self.mode = Mode::Home;
            }
        }
    }

    pub fn token(&mut self, slot: Slot) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        self.pending_submit = Some((token, slot));
        token
    }

    pub fn is_dark(&self, system: bool) -> bool {
        if self.cfg.appearance.theme_id == crate::theme::omarchy::ID {
            if let Some((id, scheme)) = crate::theme::apply::current_choice() {
                if id == crate::theme::omarchy::ID {
                    return scheme == Scheme::Dark;
                }
            }
        }
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
        perms + forms + tasks + self.external_pending().len()
    }

    pub fn external_pending(&self) -> Vec<&crate::model::fleet_attention::Pending> {
        self.fleet_pending
            .values()
            .flatten()
            .filter(|p| {
                if p.computer != self.current_computer() {
                    return true;
                }
                if let Some(id) = p.request.strip_prefix("p:") {
                    !self
                        .waiting_perms
                        .get(&p.asid)
                        .is_some_and(|list| list.iter().any(|r| r.id == id))
                } else if let Some(id) = p.request.strip_prefix("f:") {
                    !self
                        .waiting_forms
                        .get(&p.asid)
                        .is_some_and(|list| list.iter().any(|r| r.id == id))
                } else {
                    false
                }
            })
            .collect()
    }

    /// Show session `asid`. In split view it joins as a new pane (or takes
    /// focus where it already is); otherwise it fills the main card.
    pub fn open_session(&mut self, asid: &str) {
        self.route_session(asid);
        self.open_session_current(asid);
    }

    pub fn open_session_on(&mut self, computer: &str, asid: &str) {
        if self.current_computer() != computer {
            self.switch_computer(computer.to_owned());
        }
        self.open_session_current(asid);
    }

    pub fn open_notification(&mut self, key: &str) {
        if key == "desktop-update" {
            self.go(Mode::Settings);
            settings::open(self, SettingsTab::About);
            return;
        }
        if let Some(target) = key.strip_prefix("plugin:") {
            if let Ok((id, action)) = serde_json::from_str::<(String, String)>(target) {
                if let Some(plugin) = self.plugins.get_enabled(&id, &self.cfg.plugins_disabled) {
                    plugin.on_notification_action(self, &action);
                }
            }
            return;
        }
        if let Some(id) = key
            .strip_prefix("diagnostics:")
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
        {
            self.go(Mode::Settings);
            self.settings_tab = SettingsTab::Diagnostics;
            self.diagnostic_filter = Default::default();
            self.diagnostic_filter.selected = Some(id);
            return;
        }
        if let Ok((computer, asid)) = serde_json::from_str::<(String, String)>(key) {
            self.open_session_on(&computer, &asid);
        }
    }

    fn open_session_current(&mut self, asid: &str) {
        self.session_open_errors.remove(asid);
        if let Some(root) = self.sidebar_root(asid).map(str::to_owned) {
            let key = format!("{}:{root}", self.current_computer());
            self.sidebar_opened.insert(key, now_ms());
        }
        if self.sidebar_opened.len() > 256 {
            if let Some(oldest) = self
                .sidebar_opened
                .iter()
                .min_by_key(|(_, time)| *time)
                .map(|(id, _)| id.clone())
            {
                self.sidebar_opened.remove(&oldest);
            }
        }
        self.sidebar_all_sessions = false;
        self.release_plugin_page();
        if self.in_split() && self.tile_of(asid).is_none() {
            crate::platform::notify::withdraw(&crate::model::fleet_attention::notice_key(
                self.current_computer(),
                asid,
            ));
            self.open_in_tile_current(asid);
            return;
        }
        self.open_here(asid);
    }

    /// Show session `asid` in the focused pane (the main card outside split
    /// view), in place of what it showed.
    fn open_here(&mut self, asid: &str) {
        crate::platform::notify::withdraw(&crate::model::fleet_attention::notice_key(
            self.current_computer(),
            asid,
        ));
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
                tile.set_session(asid);
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
            self.reset_inspector_session_state();
        }
        self.status = format!("Opening {asid}");
        let is_optimistic = asid.starts_with("new-")
            || self
                .pending_start
                .as_ref()
                .is_some_and(|(pending, _)| pending == asid);
        if !is_optimistic {
            self.send(Command::OpenSession(AgentSessionId(asid.to_string())));
        }
        self.send(Command::Compose(
            crate::model::compose::ComposeCommand::ListPeers,
        ));
        composer::restore_attachments(self);
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

    /// Quote a block of text into the active composer draft.
    pub fn quote_to_composer(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let quoted: Vec<String> = trimmed.lines().map(|l| format!("> {l}")).collect();
        let block = format!("{}\n\n", quoted.join("\n"));
        let mut cur = self.draft.text.clone();
        if !cur.is_empty() && !cur.ends_with("\n\n") {
            if cur.ends_with('\n') {
                cur.push('\n');
            } else {
                cur.push_str("\n\n");
            }
        }
        cur.push_str(&block);
        let len = cur.len();
        self.draft = composer::fresh(cur);
        self.draft.set_caret(len);
        self.draft.focused = true;
    }

    /// Append or insert text into the active composer (`draft` or `start_draft`) and focus it.
    pub fn insert_into_composer(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if self.mode == Mode::Home && self.selected.is_none() {
            let mut cur = self.start_draft.text.clone();
            if !cur.is_empty() && !cur.ends_with('\n') && !cur.ends_with(' ') {
                cur.push('\n');
            }
            cur.push_str(trimmed);
            let len = cur.len();
            self.start_draft = composer::fresh(cur);
            self.start_draft.set_caret(len);
            self.start_draft.focused = true;
            self.quick_notes_edit.focused = false;
            self.term.sink.focused = false;
            self.persist_draft(Slot::Start);
        } else {
            let mut cur = self.draft.text.clone();
            if !cur.is_empty() && !cur.ends_with('\n') && !cur.ends_with(' ') {
                cur.push('\n');
            }
            cur.push_str(trimmed);
            let len = cur.len();
            self.draft = composer::fresh(cur);
            self.draft.set_caret(len);
            self.draft.focused = true;
            self.quick_notes_edit.focused = false;
            self.term.sink.focused = false;
            self.persist_draft(Slot::Session);
        }
    }

    /// Insert the current Quick Notes scratchpad content into the composer and focus the composer.
    pub fn insert_quick_notes_into_composer(&mut self) {
        let notes = if !self.quick_notes_edit.text.trim().is_empty() {
            self.quick_notes_edit.text.clone()
        } else {
            self.quick_notes
                .values()
                .find(|v| !v.trim().is_empty())
                .cloned()
                .unwrap_or_default()
        };
        if notes.trim().is_empty() {
            return;
        }
        self.insert_into_composer(&notes);
        if self.active_plugin_page.as_deref() == Some("quicknotes") {
            self.release_plugin_page();
        }
        self.notify(
            ToastLevel::Success,
            copy::tr_ref(self.cfg.general.language, "Inserted notes into composer"),
            None,
        );
    }

    /// Clear the Quick Notes scratchpad editor and stored tile buffers.
    pub fn clear_quick_notes(&mut self) {
        self.quick_notes_edit = TextEdit::new("");
        self.quick_notes.clear();
    }

    /// Probe external runtimes (`bun`, `herdr`) and refresh `runtime_statuses`.
    pub fn refresh_runtimes(&mut self) {
        self.runtime_statuses = muqun_gateway::platform::runtimes::probe_all();
        self.send(Command::ProbeRuntimes);
    }

    pub fn change_plugin_package(&mut self, op: crate::plugin::packages::Operation) {
        if self.plugin_packages.busy() {
            return;
        }
        if !self.runtime_statuses.iter().any(|status| {
            status.kind == muqun_gateway::platform::runtimes::RuntimeKind::Bun && status.available
        }) {
            self.toast(
                "Bun is required",
                Some("Choose Install Bun in Settings → Plugins to continue.".into()),
            );
            return;
        }
        let Some(request) = self.plugin_packages.start(op.clone()) else {
            return;
        };
        let reserved = self
            .plugins
            .all()
            .into_iter()
            .filter(|p| !self.plugins.managed_versions.contains_key(p.id()))
            .map(|p| p.id().to_owned())
            .collect();
        self.send(Command::PluginPackage(request, op, reserved));
    }

    /// Trigger background installation or update of a managed runtime (`bun` or `herdr`).
    pub fn install_managed_runtime(
        &mut self,
        kind: muqun_gateway::platform::runtimes::RuntimeKind,
    ) {
        if !self.runtime_installing.insert(kind) {
            return;
        }
        self.notify(
            ToastLevel::Info,
            format!("Installing managed {} runtime…", kind.display_name()),
            None,
        );
        self.send(Command::InstallManagedRuntime(kind));
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
        let q = self.timeline_search.query.text.trim();
        let contains = |text: &str| rune_kit::search::Fold::default().contains(text, q);
        let prose_matches = |text: &str| {
            !rune_kit::markdown::find(
                text,
                q,
                rune_kit::markdown::Case::Folded(Default::default()),
            )
            .is_empty()
        };
        self.timeline_search.matches.clear();
        self.timeline_search.active_index = 0;
        if q.is_empty() {
            return;
        }
        if let Some(tl) = self.timelines.get(asid) {
            for item in &tl.items {
                let matches = match &item.part {
                    muqun_gateway::agents::AgentPart::Text { text, .. } => prose_matches(text),
                    muqun_gateway::agents::AgentPart::Reasoning { text, .. } => prose_matches(text),
                    muqun_gateway::agents::AgentPart::Tool(tool) => {
                        contains(&tool.name)
                            || tool.title.as_ref().is_some_and(|t| contains(t))
                            || contains(&tool.input.to_string())
                            || tool
                                .output
                                .as_ref()
                                .is_some_and(|o| contains(&o.to_string()))
                    }
                    muqun_gateway::agents::AgentPart::Shell {
                        command, output, ..
                    } => contains(command) || output.as_ref().is_some_and(|text| contains(text)),
                    muqun_gateway::agents::AgentPart::Diff { file, diff } => {
                        contains(file) || contains(diff)
                    }
                    muqun_gateway::agents::AgentPart::Status { text } => contains(text),
                    muqun_gateway::agents::AgentPart::Synthetic { text, description } => {
                        contains(text) || description.as_ref().is_some_and(|d| contains(d))
                    }
                    muqun_gateway::agents::AgentPart::System { text, description } => {
                        contains(text) || description.as_ref().is_some_and(|d| contains(d))
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
        if let Some(tl) = self.timelines.get_mut(&asid) {
            let row_key = search::locate(tl, &key, "").unwrap_or_else(|| key.clone());
            if row_key != key && !tl.is_open(&row_key, false) {
                tl.flip(&row_key);
            }
            if !tl.is_open(&key, false) {
                tl.flip(&key);
            }
            tx.scroll_center_in(format!("feed-{asid}"), row_key.clone());
            tl.selected = Some(key);
            tl.flash = Some((row_key, std::time::Instant::now()));
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

    pub fn refresh_account_limits(&self) {
        for a in &self.agents {
            if a.features.extra.get("accountLimits") == Some(&serde_json::Value::Bool(true)) {
                self.send(Command::LoadAccountLimits {
                    agent: a.id.clone(),
                });
            }
        }
    }

    pub fn refresh_mcp_upstreams(&mut self) {
        if self.current_computer() == "all" {
            self.mcp_hub.loading = false;
            self.mcp_hub.error = None;
            return;
        }
        self.mcp_hub.loading = true;
        self.mcp_hub.error = None;
        self.send(Command::LoadMcpUpstreams);
    }

    pub fn go(&mut self, mode: Mode) {
        let plugin = match mode {
            Mode::Tasks => Some("tasks"),
            Mode::Mao => Some("mao"),
            Mode::Inbox => Some("inbox"),
            _ => None,
        };
        if plugin.is_some_and(|id| !crate::plugin::is_enabled(id, &self.cfg.plugins_disabled)) {
            return;
        }
        self.release_plugin_page();
        self.mode = mode;
        composer::restore_attachments(self);
        match mode {
            Mode::Tasks => self.send(Command::RefreshTasks),
            Mode::Mao => orchestration::request_gateway_sync(self),
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
                self.refresh_account_limits();
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
        self.start_catalog_failed = false;
        self.start_catalog_request = self.next_token;
        self.next_token += 1;
        self.send(Command::Compose(C::Catalog {
            request: self.start_catalog_request,
            agent: self.start_agent.clone(),
            directory,
            asid: None,
        }));
        if !cwd.is_empty() {
            self.send(Command::Compose(C::Branch { cwd: cwd.clone() }));
            if !self.demo {
                self.send(Command::WarmAgent {
                    agent: self.start_agent.clone(),
                    directory: Some(cwd),
                    auto_approve: self.cfg.ui.mao.enabled,
                });
            }
        }
    }

    /// The draft key of the composer being typed in.
    pub fn draft_key(&self, slot: Slot) -> Option<String> {
        match slot {
            Slot::Start => Some(crate::config::START_DRAFT.to_string()),
            Slot::Session => self
                .selected
                .as_deref()
                .map(|asid| self.session_draft_key(asid)),
        }
    }

    pub(crate) fn session_draft_key(&self, asid: &str) -> String {
        if self.current_computer() == "local" {
            asid.to_owned()
        } else {
            format!("computer:{}:{asid}", self.current_computer())
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
            .get(&self.session_draft_key(asid))
            .cloned()
            .unwrap_or_default();
        self.draft = TextEdit::new(text).rich();
        if !self.term.sink.focused {
            self.draft.focused = true;
        }
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

    /// Resolve the root session id for `asid` so the Session Tree always loads
    /// the full Root → Parent → Siblings → Children hierarchy even when viewing a child.
    pub fn session_tree_root_id<'a>(&'a self, asid: &'a str) -> &'a str {
        if let Some(root) = self.sidebar_root(asid) {
            return root;
        }
        let mut current = asid;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let next = self
                .session_by_id(current)
                .and_then(|info| {
                    info.parent_id
                        .as_deref()
                        .or_else(|| info.fork.as_ref().map(|f| f.session_id.as_str()))
                })
                .map(str::trim)
                .filter(|id| !id.is_empty() && *id != "null" && *id != current);
            match next {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }

    /// Ask the core for the complete session tree rooted above the current session.
    pub fn refresh_session_tree(&mut self) {
        let Some(current) = self.selected.clone() else {
            return;
        };
        let root = self.session_tree_root_id(&current).to_owned();
        self.send(Command::LoadDescendants {
            asid: AgentSessionId(root),
        });
    }

    /// The catalog loaded for `asid` (focused session or a parked tile), if any.
    pub(super) fn catalog_for_session(&self, asid: &str) -> Option<&CatalogView> {
        if self.selected.as_deref() == Some(asid) {
            return self.catalog.as_deref();
        }
        if let Some(i) = self.tile_of(asid) {
            if let Some(view) = self.tiles[i].slot.catalog.as_deref() {
                return Some(view);
            }
        }
        self.catalog.as_deref()
    }

    /// Clear session-scoped inspector state when switching sessions, tiles, or computers.
    fn reset_inspector_session_state(&mut self) {
        self.shells.clear();
        self.worktrees.clear();
        self.inspector_git_show_all = false;
        self.inspector_confirm = None;
        self.inspector_shells_expect = false;
        self.inspector_worktrees_expect = false;
    }

    /// Ask the core for what the inspector's current tab shows.
    pub fn refresh_inspector(&mut self) {
        let Some(info) = self.current().cloned() else {
            return;
        };
        let asid = info.asid.clone();
        let remote = self.is_remote_session();
        match self.inspector_tab {
            InspectorTab::Queue => {}
            InspectorTab::Git => {
                if let Some(directory) = info.directory.clone() {
                    self.inspector_worktrees_expect = true;
                    self.send(Command::LoadWorktrees {
                        agent: info.agent_id.clone(),
                        directory: directory.clone(),
                    });
                    if !remote {
                        self.send(Command::Compose(
                            crate::model::compose::ComposeCommand::Branch { cwd: directory },
                        ));
                    }
                }
                self.send(Command::RefreshDiff(asid));
            }
            InspectorTab::Subagents => self.refresh_session_tree(),
            InspectorTab::Saved => self.send(Command::LoadSavedPermissions { asid }),
            InspectorTab::Shells => {
                self.inspector_shells_expect = true;
                self.send(Command::LoadShells {
                    agent: info.agent_id.clone(),
                    directory: info.directory.clone(),
                });
            }
        }
    }

    pub fn close_inspector(&mut self) {
        self.close_plugin_drawer();
    }

    pub fn toggle_inspector(&mut self) {
        if self.right_open
            && self
                .active_plugin_drawer
                .as_deref()
                .is_none_or(|id| id == "inspector")
        {
            self.close_inspector();
        } else {
            self.open_plugin_drawer("inspector");
        }
    }

    pub fn open_git_inspector(&mut self) {
        if !crate::plugin::is_enabled("inspector", &self.cfg.plugins_disabled) {
            return;
        }
        if self.mode != Mode::Session && self.selected.is_some() {
            self.go(Mode::Session);
        }
        self.inspector_tab = InspectorTab::Git;
        self.open_plugin_drawer("inspector");
    }

    pub fn toggle_git_inspector(&mut self) {
        if self.mode != Mode::Session && self.selected.is_some() {
            self.go(Mode::Session);
        }
        if self.right_open
            && self
                .active_plugin_drawer
                .as_deref()
                .is_none_or(|id| id == "inspector")
            && self.inspector_tab == InspectorTab::Git
            && !self.term.open
        {
            self.close_inspector();
        } else {
            self.open_git_inspector();
        }
    }

    fn request_session_catalog(&mut self, info: &AgentSessionInfo) {
        let request = self.next_token;
        if self.selected.as_deref() == Some(&info.asid.0) {
            self.catalog_request = request;
        } else if let Some(tile) = self
            .tiles
            .iter_mut()
            .find(|t| t.asid.as_deref() == Some(&info.asid.0))
        {
            tile.slot.catalog_request = request;
        } else {
            return;
        }
        self.next_token += 1;
        self.send(Command::Compose(
            crate::model::compose::ComposeCommand::Catalog {
                request,
                agent: info.agent_id.clone(),
                directory: info.directory.clone(),
                asid: Some(info.asid.clone()),
            },
        ));
    }

    /// The composer's catalogue, branch and bypass state for a freshly opened session.
    pub(super) fn session_opened(&mut self, info: &AgentSessionInfo) {
        if self
            .pending_start
            .as_ref()
            .is_some_and(|(pending, _)| pending == &info.asid.0)
        {
            return;
        }
        use crate::model::compose::ComposeCommand as C;
        self.request_session_catalog(info);
        self.send(Command::Compose(C::AutoApprove {
            asid: info.asid.clone(),
        }));
        if !self.is_remote_session() {
            if let Some(cwd) = info.directory.clone() {
                self.send(Command::Compose(C::Branch { cwd }));
            }
        }
        self.send(Command::Compose(C::ListPeers));
        if self.right_open {
            self.refresh_inspector();
        }
    }

    // ---- split tiles ---------------------------------------------------------------

    /// Render the tile workspace, including a standalone plugin or terminal window.
    pub fn in_split(&self) -> bool {
        self.tiles.len() >= 2
            || self.tiles.first().is_some_and(|tile| {
                matches!(
                    tile.content,
                    TileContent::Custom { .. } | TileContent::Terminal
                )
            })
    }

    pub fn tile_of(&self, asid: &str) -> Option<usize> {
        self.tiles
            .iter()
            .position(|t| t.asid.as_deref() == Some(asid))
    }

    /// The terminal tile showing the live (active) shell: the focused terminal
    /// tile, else the one bound to the active tab, else the first terminal tile.
    /// Exactly one terminal tile is live, so only it carries the keyboard sink.
    pub fn tile_of_terminal(&self) -> Option<usize> {
        if self.tiles.get(self.focus).is_some_and(Tile::is_terminal) {
            return Some(self.focus);
        }
        let active = self.term.active_tab;
        self.tiles
            .iter()
            .position(|t| t.is_terminal() && t.term_tab.is_none_or(|tab| tab == active))
            .or_else(|| self.tiles.iter().position(Tile::is_terminal))
    }

    /// The terminal tab a non-live terminal tile shows, when it is bound to one.
    pub fn tile_terminal_tab(&self, i: usize) -> Option<usize> {
        let tile = self.tiles.get(i).filter(|t| t.is_terminal())?;
        if self.tile_of_terminal() == Some(i) {
            return Some(self.term.active_tab);
        }
        tile.term_tab.filter(|tab| *tab < self.term.tabs.len())
    }

    pub fn has_terminal_tile(&self) -> bool {
        self.tiles.iter().any(|t| t.is_terminal())
    }

    /// Move the focused session's view state out of the top-level fields.
    fn take_slot(&mut self) -> SessionSlot {
        SessionSlot {
            draft: std::mem::replace(&mut self.draft, TextEdit::new("").rich()),
            queue: std::mem::take(&mut self.queue),
            staged: std::mem::take(&mut self.staged),
            diff_stat: std::mem::take(&mut self.diff_stat),
            diff: self.diff.take(),
            show_diff: std::mem::replace(&mut self.show_diff, false),
            diff_active_file: std::mem::take(&mut self.diff_active_file),
            revert_staged: std::mem::replace(&mut self.revert_staged, false),
            catalog: self.catalog.take(),
            catalog_request: std::mem::take(&mut self.catalog_request),
            descendants: std::mem::take(&mut self.descendants),
            saved_permissions: std::mem::take(&mut self.saved_permissions),
        }
    }

    fn put_slot(&mut self, slot: SessionSlot) {
        self.draft = slot.draft;
        self.queue = slot.queue;
        self.staged = slot.staged;
        self.diff_stat = slot.diff_stat;
        self.diff = slot.diff;
        self.show_diff = slot.show_diff;
        self.diff_active_file = slot.diff_active_file;
        self.revert_staged = slot.revert_staged;
        self.catalog = slot.catalog;
        self.catalog_request = slot.catalog_request;
        self.descendants = slot.descendants;
        self.saved_permissions = slot.saved_permissions;
    }

    /// Route native keyboard focus after activating a split pane.
    pub(super) fn focus_pane_input(&mut self, tx: &mut rune::Tx) {
        match self.tiles.get(self.focus).map(|tile| &tile.content) {
            Some(TileContent::Session(_)) if !self.timeline_search.active => {
                tx.focus("draft", false)
            }
            Some(TileContent::Terminal) => self.focus_terminal_tx(tx),
            _ => {}
        }
    }

    /// Give tile `i` the keyboard: its session or window becomes active,
    /// and the old focused tile keeps its state in its slot.
    pub fn focus_tile(&mut self, i: usize) {
        if i >= self.tiles.len() {
            self.mode = Mode::Session;
            composer::restore_attachments(self);
            return;
        }
        if i == self.focus {
            self.mode = Mode::Session;
            match &self.tiles[i].content {
                TileContent::Session(asid) => {
                    if self.selected.as_ref() != Some(asid) {
                        self.selected = Some(asid.clone());
                        self.sync_watch();
                        if self.catalog.is_none() {
                            if let Some(info) = self.current().cloned() {
                                self.session_opened(&info);
                            }
                        }
                    }
                    self.draft.focused = true;
                    self.term.sink.focused = false;
                }
                TileContent::Terminal => {
                    self.term.sink.focused = true;
                    self.draft.focused = false;
                    self.sync_tile_terminal_tab(i);
                }
                _ => {}
            }
            composer::restore_attachments(self);
            return;
        }
        let old = self.take_slot();
        self.tiles[self.focus].slot = old;
        let new = std::mem::take(&mut self.tiles[i].slot);
        self.put_slot(new);
        self.focus = i;
        self.selected = self.tiles[i].asid.clone();
        if self.timeline_search.active {
            if let Some(asid) = self.selected.clone() {
                self.update_timeline_search_matches(&asid);
            } else {
                self.close_timeline_search();
            }
        }
        self.menu = None;
        self.context_open = None;
        self.reset_inspector_session_state();
        self.mode = Mode::Session;

        match &self.tiles[i].content {
            TileContent::Session(_) => {
                self.draft.focused = true;
                self.term.sink.focused = false;
                self.sync_watch();
                if self.catalog.is_none() {
                    if let Some(info) = self.current().cloned() {
                        self.session_opened(&info);
                    }
                } else if self.right_open {
                    self.refresh_inspector();
                }
            }
            TileContent::Terminal => {
                self.term.sink.focused = true;
                self.draft.focused = false;
                if self.term.session.is_none() && self.term.tabs.is_empty() {
                    self.start_terminal();
                }
                self.sync_tile_terminal_tab(i);
            }
            TileContent::Empty => {
                self.draft.focused = false;
                self.term.sink.focused = false;
            }
            _ => {}
        }
        composer::restore_attachments(self);
    }

    /// Ensure `s.term.active_tab` matches the terminal tab bound to tile `i`.
    pub(crate) fn sync_tile_terminal_tab(&mut self, i: usize) {
        let Some(tile) = self.tiles.get(i) else {
            return;
        };
        if !tile.is_terminal() {
            return;
        }
        if let Some(tab_idx) = tile.term_tab {
            if tab_idx < self.term.tabs.len() {
                if self.term.active_tab != tab_idx {
                    if let Some(cur) = self.term.tabs.get_mut(self.term.active_tab) {
                        cur.session = self.term.session.clone();
                        cur.frame = self.term.frame.clone();
                    }
                    self.term.active_tab = tab_idx;
                    self.term.session = self.term.tabs[tab_idx].session.clone();
                    self.term.frame = self.term.tabs[tab_idx].frame.clone();
                }
                return;
            }
        }
        self.tiles[i].term_tab = Some(self.term.active_tab);
    }

    /// Tell the core which sessions are on screen in split tiles, and who is focused.
    fn sync_watch(&self) {
        let mut watched: Vec<AgentSessionId> = self
            .tiles
            .iter()
            .filter_map(|t| t.asid.clone())
            .filter(|asid| !asid.starts_with("new-"))
            .map(AgentSessionId)
            .collect();
        for asid in self.orchestration.tracked_asids() {
            if !watched.contains(&asid) {
                watched.push(asid);
            }
        }
        self.send(Command::WatchSessions(watched));
        if let Some(ref asid) = self.selected {
            if !asid.starts_with("new-") {
                self.send(Command::FocusSession(AgentSessionId(asid.clone())));
            }
        }
    }

    /// Open `asid` beside the current session(s): it fills an empty pane,
    /// or splits the focused pane (Hyprland's dwindle), or at the most
    /// panes takes the focused pane's place. The pane it lands in takes
    /// focus; a session already in a pane just takes focus.
    pub fn open_in_tile(&mut self, asid: &str) {
        self.route_session(asid);
        self.open_in_tile_current(asid);
    }

    fn open_in_tile_current(&mut self, asid: &str) {
        self.session_open_errors.remove(asid);
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
            self.tiles.push(Tile::session(current));
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }
        let at = match self.tiles.iter().position(|t| t.is_empty()) {
            Some(empty) => empty,
            None if self.tiles.len() < MAX_TILES => self.add_tile(),
            None => {
                self.open_here(asid);
                return;
            }
        };
        self.tiles[at].set_session(asid);
        self.tiles[at].slot = SessionSlot {
            draft: TextEdit::new(
                self.cfg
                    .general
                    .drafts
                    .get(&self.session_draft_key(asid))
                    .cloned()
                    .unwrap_or_default(),
            )
            .rich(),
            ..SessionSlot::default()
        };
        self.mode = Mode::Session;
        let is_optimistic = asid.starts_with("new-")
            || self
                .pending_start
                .as_ref()
                .is_some_and(|(pending, _)| pending == asid);
        if !is_optimistic {
            self.send(Command::OpenTile(AgentSessionId(asid.to_string())));
        }
        self.focus_tile(at);
        self.sync_watch();
        self.persist_tiles();
    }

    /// Open the terminal in a split tile beside the current view.
    /// If a terminal tile is already focused, splits a new independent terminal tile beside it.
    pub fn open_terminal_tile(&mut self) {
        if let Some(i) = self.tile_of_terminal() {
            if self.in_split() && self.focus == i {
                self.split_terminal_tile();
            } else {
                self.focus_tile(i);
            }
            return;
        }
        if self.term.open {
            self.close_terminal();
        } else {
            self.remove_dock_tab("terminal", false);
        }

        if self.tiles.is_empty() {
            let current = self.selected.clone();
            self.tiles.push(Tile {
                content: match &current {
                    Some(asid) => TileContent::Session(asid.clone()),
                    None => TileContent::Empty,
                },
                asid: current,
                ..Tile::default()
            });
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }

        let at = match self.tiles.iter().position(|t| t.is_empty()) {
            Some(empty) => empty,
            None if self.tiles.len() < MAX_TILES => self.add_tile(),
            None => self.focus,
        };

        if self.term.session.is_none() && self.term.tabs.is_empty() {
            self.start_terminal();
        }
        self.tiles[at].set_terminal_tab(self.term.active_tab);
        self.mode = Mode::Session;
        self.focus_tile(at);
        self.persist_tiles();
    }

    /// Split a new independent terminal tile beside the currently focused tile,
    /// assigning it an unused terminal tab or spawning a new terminal tab for it.
    pub fn split_terminal_tile(&mut self) {
        if !self.has_terminal_tile() {
            self.open_terminal_tile();
            return;
        }
        self.close_terminal();
        self.right_open = false;
        self.right.set_open(false);

        let at = match self.tiles.iter().position(|t| t.is_empty()) {
            Some(empty) => empty,
            None if self.tiles.len() < MAX_TILES => self.add_tile(),
            None => return,
        };
        tiles::fill_terminal_tile(self, at);
    }

    /// An empty tile splitting the focused one; its index.
    fn add_tile(&mut self) -> usize {
        let mut tree = self.tile_layout();
        let new = self.tiles.len();
        dwindle::insert(&mut tree, self.focus, new, self.tile_grid);
        self.tile_tree = tree;
        let mut tile = Tile::empty();
        if self.cfg.ui.tile_mode == crate::config::TileLayoutMode::Floating {
            tile.floating_rect = Some(Self::default_floating_rect(new, self.tile_grid));
        }
        self.tiles.push(tile);
        new
    }

    /// Default cascaded floating window rect for tile `i` within `stage`.
    pub fn default_floating_rect(i: usize, stage: [f32; 2]) -> [f32; 4] {
        let sw = if stage[0] > 100.0 { stage[0] } else { 1120.0 };
        let sh = if stage[1] > 100.0 { stage[1] } else { 760.0 };
        let w = (sw * 0.56).clamp(420.0, (sw - 48.0).max(420.0));
        let h = (sh * 0.68).clamp(320.0, (sh - 48.0).max(320.0));
        let step = (i % 6) as f32 * 32.0;
        let x = (36.0 + step).min((sw - w - 16.0).max(16.0));
        let y = (28.0 + step).min((sh - h - 16.0).max(16.0));
        [x, y, w, h]
    }

    /// Active tile layout mode (`Dwindle`, `Scrolling`, or `Floating`).
    pub fn tile_layout_mode(&self) -> crate::config::TileLayoutMode {
        self.cfg.ui.tile_mode
    }

    /// Switch the workspace tile layout mode (`Dwindle`, `Scrolling`, `Floating`).
    pub fn set_tile_layout_mode(&mut self, mode: crate::config::TileLayoutMode) {
        self.zoomed_tile = None;
        if mode == crate::config::TileLayoutMode::Floating {
            let grid = self.tile_grid;
            for (i, t) in self.tiles.iter_mut().enumerate() {
                if t.floating_rect.is_none() {
                    t.floating_rect = Some(Self::default_floating_rect(i, grid));
                }
            }
        }
        self.edit(ConfigEdit::TileMode(mode));
        let language = self.cfg.general.language;
        let msg = format!(
            "{}: {}",
            copy::tr(language, "Layout"),
            copy::tr(language, mode.label())
        );
        self.toast(msg, None);
    }

    /// Cycle through Omarchy 4.5 workspace layout modes (`Dwindle` → `Scrolling` → `Floating`).
    pub fn cycle_tile_layout(&mut self) {
        let next = self.cfg.ui.tile_mode.next();
        self.set_tile_layout_mode(next);
    }

    /// Cycle backward through workspace layout modes.
    pub fn cycle_tile_layout_prev(&mut self) {
        let prev = self.cfg.ui.tile_mode.prev();
        self.set_tile_layout_mode(prev);
    }

    /// Flip the orientation of the focused tile's parent split (Omarchy `Super+J`).
    pub fn toggle_tile_split_axis(&mut self) {
        if !self.in_split() {
            return;
        }
        let mut tree = self.tile_layout();
        if dwindle::toggle_parent_split(&mut tree, self.focus) {
            self.tile_tree = tree;
            self.persist_tiles();
        }
    }

    /// Reset all split ratios in the tile tree to 50/50.
    pub fn equalize_tiles(&mut self) {
        if !self.in_split() {
            return;
        }
        let mut tree = self.tile_layout();
        dwindle::equalize(&mut tree);
        self.tile_tree = tree;
        self.persist_tiles();
    }

    /// Split the focused tile explicitly side-by-side (`across = true`) or stacked (`across = false`).
    pub fn split_tile_with_axis(&mut self, across: bool) {
        if self.tiles.len() >= MAX_TILES {
            return;
        }
        self.right_open = false;
        self.right.set_open(false);
        self.zoomed_tile = None;
        if self.tiles.is_empty() {
            let Some(current) = self.selected.clone() else {
                return;
            };
            self.tiles.push(Tile::session(current));
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }
        let mut tree = self.tile_layout();
        let new = self.tiles.len();
        dwindle::insert_with_axis(&mut tree, self.focus, new, across);
        self.tile_tree = tree;
        let mut tile = Tile::empty();
        if self.cfg.ui.tile_mode == crate::config::TileLayoutMode::Floating {
            tile.floating_rect = Some(Self::default_floating_rect(new, self.tile_grid));
        }
        self.tiles.push(tile);
        self.mode = Mode::Session;
        self.focus_tile(new);
    }

    /// Maximize / restore the focused tile (Omarchy `Super+Alt+F`).
    pub fn toggle_tile_zoom(&mut self) {
        self.toggle_tile_zoom_at(self.focus);
    }

    /// Maximize / restore tile `i`.
    pub fn toggle_tile_zoom_at(&mut self, i: usize) {
        if !self.in_split() || i >= self.tiles.len() {
            self.zoomed_tile = None;
            return;
        }
        if self.zoomed_tile == Some(i) {
            self.zoomed_tile = None;
        } else {
            self.focus_tile(i);
            self.zoomed_tile = Some(i);
        }
    }

    /// Pop out / un-pop the focused tile as a pinned floating overlay window (Omarchy `Super+O`).
    pub fn toggle_tile_pop(&mut self) {
        self.toggle_tile_pop_at(self.focus);
    }

    /// Pop out / un-pop tile `i` as a pinned floating overlay window.
    pub fn toggle_tile_pop_at(&mut self, i: usize) {
        if !self.in_split() || i >= self.tiles.len() {
            return;
        }
        let grid = self.tile_grid;
        let next = !self.tiles[i].popped;
        // Ensure at least one tile remains un-popped in the background.
        let unpopped_others = self
            .tiles
            .iter()
            .enumerate()
            .filter(|(idx, t)| *idx != i && !t.popped)
            .count();
        if next && unpopped_others == 0 {
            self.set_tile_layout_mode(crate::config::TileLayoutMode::Floating);
            return;
        }
        self.tiles[i].popped = next;
        if next && self.tiles[i].floating_rect.is_none() {
            self.tiles[i].floating_rect = Some(Self::default_floating_rect(i, grid));
        }
        self.focus_tile(i);
    }

    /// Complete a drag-and-drop of tile `src` onto tile `dst` in `zone`.
    pub fn drop_tile_on(&mut self, src: usize, dst: usize, zone: dwindle::DropZone) {
        if src >= self.tiles.len() || dst >= self.tiles.len() || src == dst {
            return;
        }
        if zone == dwindle::DropZone::Center {
            self.swap_tiles(src, dst);
        } else {
            let tree = self.tile_layout();
            self.tile_tree = dwindle::move_and_split(&tree, src, dst, zone);
            self.focus_tile(src);
            self.persist_tiles();
        }
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
        self.zoomed_tile = None;
        if self.tiles.is_empty() {
            let Some(current) = self.selected.clone() else {
                return;
            };
            self.tiles.push(Tile::session(current));
            self.focus = 0;
            self.tile_tree = crate::config::TileNode::Leaf(0);
        }
        let new = self.add_tile();
        self.mode = Mode::Session;
        self.focus_tile(new);
    }

    /// Leave split view: keep the focused pane's session, close the others.
    pub fn unsplit(&mut self) {
        self.zoomed_tile = None;
        self.tile_card_drag = None;
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
        self.tile_card_drag = None;
        if let Some(z) = self.zoomed_tile {
            if z == i {
                self.zoomed_tile = None;
            } else if z > i {
                self.zoomed_tile = Some(z - 1);
            }
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
        if let TileContent::Custom { id, kind, .. } = &gone.content {
            self.release_plugin_view(kind, id);
        }
        self.tile_tree = dwindle::remove(self.tile_layout_of(self.tiles.len() + 1), i)
            .unwrap_or(crate::config::TileNode::Leaf(0));
        if self.focus > i {
            self.focus -= 1;
        }
        if let Some(asid) = gone.asid.as_deref() {
            // Closing a pane/window must never cause its session to vanish from
            // the Recent sessions sidebar: bump its recent-opened timestamp so
            // it stays right at the top for immediate re-entry.
            if let Some(root) = self.sidebar_root(asid).map(str::to_owned) {
                let key = format!("{}:{root}", self.current_computer());
                self.sidebar_opened.insert(key, now_ms());
            }
            if self.selected.as_deref() != Some(asid) {
                self.timelines.remove(asid);
            }
        }
        if let Some(asid) = gone.asid {
            let key = self.session_draft_key(&asid);
            let text = gone.slot.draft.text.clone();
            let stored = self.cfg.general.drafts.get(&key).map_or("", String::as_str);
            if stored != text {
                self.edit(ConfigEdit::Draft { key, text });
            }
        }
        if self.tiles.len() <= 1 {
            self.zoomed_tile = None;
            let last_tile = self.tiles.pop();
            self.tiles.clear();
            self.tile_tree = crate::config::TileNode::Leaf(0);
            self.focus = 0;
            if let Some(tile) = last_tile {
                if let Some(asid) = tile.asid {
                    self.open_session(&asid);
                } else if tile.is_terminal() {
                    self.open_terminal();
                } else if let TileContent::Custom { kind, .. } = tile.content {
                    self.plugin_page_origin = None;
                    self.open_plugin_page(&kind);
                }
            } else if self.selected.is_none() {
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
        if self.zoomed_tile == Some(i) {
            self.zoomed_tile = Some(j);
        } else if self.zoomed_tile == Some(j) {
            self.zoomed_tile = Some(i);
        }
        self.menu = None;
        self.persist_tiles();
    }

    /// Remember the split for the next launch: its sessions and their
    /// tiling, empty panes left out.
    pub fn persist_tiles(&mut self) {
        if self.pending_start.is_some() {
            return;
        }
        let asids: Vec<String> = self
            .tiles
            .iter()
            .filter_map(|t| t.content.to_persist_key())
            .collect();
        let mut layout = (self.tiles.len() >= 2).then(|| self.tile_layout());
        for (i, t) in self.tiles.iter().enumerate().rev() {
            if t.is_empty() {
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
            .filter(|key| {
                if key.as_str() == "terminal" {
                    true
                } else if let Some(asid) = key.strip_prefix("session:") {
                    self.session_by_id(asid).is_some()
                } else {
                    self.session_by_id(key).is_some()
                }
            })
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
        self.tiles.clear();
        let mut term_idx = 0usize;
        for key in &saved {
            match TileContent::from_persist_key(key) {
                TileContent::Terminal => {
                    let mut tile = Tile::terminal();
                    tile.term_tab = Some(term_idx);
                    term_idx += 1;
                    self.tiles.push(tile);
                }
                TileContent::Session(asid) => self.tiles.push(Tile::session(asid)),
                TileContent::Diff(path) => self.tiles.push(Tile {
                    content: TileContent::Diff(path),
                    ..Tile::default()
                }),
                TileContent::Custom { id, kind, title } => self.tiles.push(Tile {
                    content: TileContent::Custom { id, kind, title },
                    ..Tile::default()
                }),
                TileContent::Empty => self.tiles.push(Tile::empty()),
            }
        }
        if let Some(first_asid) = self.tiles.iter().find_map(|t| t.asid.clone()) {
            self.open_session(&first_asid);
            // Watching only subscribes to future events. Every restored pane
            // needs its initial snapshot, including panes nobody has clicked.
            for asid in self
                .tiles
                .iter()
                .filter_map(|tile| tile.asid.clone())
                .collect::<Vec<_>>()
            {
                if let Some(root) = self.sidebar_root(&asid).map(str::to_owned) {
                    let key = format!("{}:{root}", self.current_computer());
                    self.sidebar_opened.insert(key, now_ms());
                }
                if asid != first_asid && !asid.starts_with("new-") {
                    self.send(Command::OpenTile(AgentSessionId(asid)));
                }
            }
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
                Update::Computer {
                    computer,
                    generation,
                    updates,
                } => {
                    if self.forgotten_computers.contains(&computer) {
                        continue;
                    }
                    let latest = self
                        .computer_generations
                        .entry(computer.clone())
                        .or_default();
                    let stale = generation < *latest;
                    *latest = (*latest).max(generation);
                    let active = self.current_computer() == computer;
                    for update in updates {
                        let receipt = matches!(&update, Update::SessionCreated(..))
                            || matches!(&update, Update::Composer(composer)
                                if matches!(**composer, ComposerUpdate::Submitted { .. }));
                        if receipt {
                            if active {
                                self.apply_updates(vec![update]);
                            } else {
                                self.computer_data
                                    .entry(computer.clone())
                                    .or_default()
                                    .pending_receipts
                                    .push(update);
                            }
                            continue;
                        }
                        if stale {
                            continue;
                        }
                        if active {
                            self.apply_updates(vec![update]);
                        } else if let Update::AgentSessions { agent, sessions } = update {
                            let children: HashSet<_> = self
                                .computer_data
                                .get(&computer)
                                .into_iter()
                                .flat_map(|data| &data.sessions)
                                .filter(|info| self.is_subagent_session_on(&computer, info))
                                .map(|info| info.asid.0.clone())
                                .collect();
                            for info in &sessions {
                                self.remember_session_lineage_on(&computer, info);
                            }
                            let data = self.computer_data.entry(computer.clone()).or_default();
                            data.sessions
                                .retain(|s| s.agent_id != agent || children.contains(&s.asid.0));
                            data.sessions.extend(sessions);
                            if self.current_computer() == "all" {
                                self.rebuild_all_sessions();
                            }
                        }
                    }
                }
                Update::SessionOpenFailed { asid, message } => {
                    self.session_open_errors.insert(asid, message);
                }
                Update::Agents(agents) => {
                    self.on_agents(agents);
                    self.restore_tiles();
                }
                Update::ComputerPairingRequested {
                    attempt,
                    url,
                    result,
                } => {
                    if self.pairing_target.is_none()
                        || !self.pairing_busy
                        || self.pairing_attempt != attempt
                    {
                        continue;
                    }
                    self.pairing_busy = false;
                    match result {
                        Ok(offer) => {
                            self.pairing_url_input = TextEdit::new(url);
                            self.pairing_offer = Some(offer);
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
                        Err(error) => self.pairing_error = Some(error),
                    }
                }
                Update::ComputerPairingOutcome {
                    attempt,
                    url,
                    result,
                } => {
                    if self.pairing_target.is_none()
                        || !self.pairing_busy
                        || self.pairing_attempt != attempt
                    {
                        continue;
                    }
                    self.pairing_busy = false;
                    match result {
                        Ok(claim) => {
                            self.close_pairing();
                            let server_id = claim.server_id;
                            self.forgotten_computers.remove(&server_id);
                            self.computer_unpair.invalidate(&server_id);
                            self.edit(ConfigEdit::ComputerToken {
                                computer: server_id.clone(),
                                label: claim.server_label,
                                url,
                                token: claim.token,
                                device_id: claim.device_id,
                                transport_key: claim.transport_key,
                            });
                            self.toast(
                                format!(
                                    "{}: {}",
                                    self.computer_name(&server_id),
                                    copy::tr(self.cfg.general.language, "Connected successfully")
                                ),
                                None,
                            );
                            self.switch_computer(server_id);
                        }
                        Err(error) => self.pairing_error = Some(error),
                    }
                }
                Update::AgentSessions { agent, sessions } => {
                    self.listed_agents.insert(agent.clone());
                    let curr = self.current_computer().to_string();
                    if curr != "all" {
                        let children: HashSet<_> = self
                            .sessions
                            .iter()
                            .filter(|info| self.is_subagent_session(info))
                            .map(|info| info.asid.0.clone())
                            .collect();
                        let keep_active: HashSet<String> = self
                            .sessions
                            .iter()
                            .filter(|s| {
                                s.agent_id == agent
                                    && (matches!(
                                        self.activity(s),
                                        Activity::Working | Activity::NeedsYou
                                    ) || self.selected.as_deref() == Some(s.asid.0.as_str())
                                        || self.tile_of(&s.asid.0).is_some())
                            })
                            .map(|s| s.asid.0.clone())
                            .collect();
                        self.sessions.retain(|s| {
                            s.agent_id != agent
                                || children.contains(&s.asid.0)
                                || keep_active.contains(&s.asid.0)
                                || self
                                    .pending_start
                                    .as_ref()
                                    .is_some_and(|(pending, _)| pending == &s.asid.0)
                        });
                        self.merge_sessions(sessions);
                        if self.error.as_ref().is_some_and(|e| e.unreachable) {
                            self.error = None;
                        }
                        self.restore_tiles();
                    } else {
                        // Aggregate lists must carry their source computer; an
                        // unscoped list cannot safely be assigned to local.
                        self.rebuild_all_sessions();
                    }
                }
                Update::SessionCreated(info, start_token) => {
                    let asid = info.asid.clone();
                    self.merge_sessions(vec![(*info).clone()]);
                    if let Some(token) = start_token {
                        orchestration::on_session_created(self, token, &info);
                        if self
                            .pending_start
                            .as_ref()
                            .is_some_and(|(_, t)| *t == token)
                        {
                            self.adopt_created_session(&asid.0);
                            if self.mode == Mode::Session
                                && self.selected.as_deref() == Some(&asid.0)
                            {
                                self.send(Command::FocusSession(asid));
                                self.session_opened(&info);
                            } else if self.tile_of(&asid.0).is_some() && self.mode == Mode::Session
                            {
                                self.send(Command::OpenTile(asid));
                            }
                        }
                    } else {
                        self.open_session_current(&asid.0);
                    }
                }
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
                Update::Error(mut err) => {
                    self.compacting_sessions.clear();
                    err.message = crate::model::inbox::clean_error_message(&err.message);
                    self.status = err.message.clone();
                    self.error_at = Some(std::time::Instant::now());
                    self.error = Some(err);
                    HAS_TOASTS.store(true, Ordering::Release);
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
                Update::RemoteFileLoaded { path, result } => self.remote_file_loaded(&path, result),
                Update::AccountLimits { agent, limits } => {
                    self.account_limits.insert(agent, limits);
                }
                Update::Earlier {
                    asid,
                    items,
                    exhausted,
                } => {
                    if self.timelines.contains_key(&asid) {
                        self.remember_timeline_lineage(&asid, &items);
                    }
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
                    orchestration::poll_active_gateway_runs(self, true);
                }
                Update::TasksFailed(why) => self.tasks_view.error = Some(why),
                Update::McpUpstreamsLoaded { result } => self.mcp_hub.apply_loaded(result),
                Update::McpUpstreamSaved { id, result } => self.mcp_hub.apply_saved(id, result),
                Update::McpUpstreamDeleted { id, result } => self.mcp_hub.apply_deleted(id, result),
                Update::McpUpstreamProbed { id, result } => self.mcp_hub.apply_probed(id, result),
                Update::TaskHistory { id, steps } => {
                    self.tasks_view.history.insert(id, steps);
                }
                Update::Descendants { asid, sessions } => {
                    for info in &sessions {
                        self.remember_session_lineage(info);
                        self.remember_subagent_context(
                            &info.asid.0,
                            info.parent_id.as_deref().or(Some(&asid)),
                        );
                    }
                    let matches_selected = self
                        .selected
                        .as_deref()
                        .is_some_and(|sel| sel == asid || self.session_tree_root_id(sel) == asid);
                    if matches_selected {
                        self.descendants = sessions.clone();
                    }
                    let matching_tiles: Vec<usize> = self
                        .tiles
                        .iter()
                        .enumerate()
                        .filter(|(i, tile)| {
                            (*i != self.focus || !matches_selected)
                                && tile.asid.as_deref().is_some_and(|tid| {
                                    tid == asid || self.session_tree_root_id(tid) == asid
                                })
                        })
                        .map(|(i, _)| i)
                        .collect();
                    for idx in matching_tiles {
                        self.tiles[idx].slot.descendants = sessions.clone();
                    }
                }
                Update::SavedPermissions { asid, items } => {
                    if self.selected.as_deref() == Some(asid.as_str()) {
                        self.saved_permissions = items;
                    } else if let Some(slot) = self.parked_slot(&asid) {
                        slot.saved_permissions = items;
                    }
                }
                Update::Shells(rows) => {
                    if self.inspector_shells_expect {
                        self.shells = rows;
                    }
                }
                Update::Worktrees(rows) => {
                    if self.inspector_worktrees_expect {
                        self.worktrees = rows;
                    }
                }
                Update::HubMode(mode) => self.hub_label = mode.to_string(),
                Update::HubLink(down) => self.hub_down = down,
                Update::Search {
                    generation,
                    outcome,
                } => self.on_search(generation, outcome),
                Update::FleetAttention(change) => {
                    if !self.forgotten_computers.contains(&change.computer) {
                        self.on_fleet_attention(*change);
                    }
                }
                Update::FleetAttentionUnavailable(computer) => {
                    if self.forgotten_computers.contains(&computer) {
                        continue;
                    }
                    self.toast(format!("{} · {}", self.computer_name(&computer), copy::tr(self.cfg.general.language, "Gateway update required")), Some(copy::tr(self.cfg.general.language, "Update this computer’s Gateway to recover approvals and questions after reconnecting. Live requests still appear.")));
                }
                Update::Voice(event) => self.on_voice(event),
                Update::PluginPackageProgress(request, stage) => {
                    self.plugin_packages.advance(request, stage)
                }
                Update::PluginPackage(request, event, result) => {
                    if !self.plugin_packages.finish(request, &result) {
                        continue;
                    }
                    match result {
                        Ok(id) => {
                            self.plugins.reload_package(&id);
                            if self.plugins.get(&id).is_none() {
                                self.close_plugin_windows(&id);
                            }
                            self.toast("Plugin packages updated", Some(id));
                        }
                        Err(err) => self.notify_diagnostic(
                            event,
                            ToastLevel::Error,
                            "Plugin operation failed",
                            Some(err),
                        ),
                    }
                }
                Update::Runtimes(statuses) => {
                    self.runtime_statuses = statuses;
                }
                Update::RuntimeInstalled {
                    kind,
                    result,
                    event,
                } => {
                    self.runtime_installing.remove(&kind);
                    match result {
                        Ok(status) => {
                            let ver = status.version.clone().map(|v| {
                                format!("v{v} installed in {}", status.managed_dir.display())
                            });
                            if let Some(slot) =
                                self.runtime_statuses.iter_mut().find(|s| s.kind == kind)
                            {
                                *slot = status;
                            } else {
                                self.runtime_statuses.push(status);
                            }
                            self.notify(
                                ToastLevel::Success,
                                format!("Managed {} runtime ready", kind.display_name()),
                                ver,
                            );
                        }
                        Err(err) => {
                            self.notify_diagnostic(
                                event,
                                ToastLevel::Error,
                                format!("Failed to install {}", kind.display_name()),
                                Some(err),
                            );
                        }
                    }
                }
                Update::Orchestration {
                    computer,
                    ticket,
                    result,
                } => orchestration::on_gateway_orchestration(self, &computer, ticket, result),
                Update::MaoBaselineCaptured {
                    run_id,
                    base_commit,
                    base_commits,
                } => orchestration::on_baseline_captured(self, &run_id, base_commit, base_commits),
                Update::MaoReviewReady {
                    run_id,
                    diff_summary,
                    files,
                } => orchestration::on_review_ready(self, &run_id, diff_summary, files),
            }
        }
        if LAYOUT_DIRTY.swap(false, Ordering::AcqRel) {
            self.persist_layout();
        }
        // Keep streaming tool rows stable throughout a turn; fold them once
        // after its final status, including snapshots and parked split panes.
        for info in &self.sessions {
            if matches!(
                info.status,
                AgentSessionStatus::Idle
                    | AgentSessionStatus::Failed
                    | AgentSessionStatus::Interrupted
            ) {
                if let Some(tl) = self.timelines.get_mut(&info.asid.0) {
                    tl.settle_activity();
                }
            }
        }
        self.note_busy();
        inbox::note_waiting(self);
        orchestration::poll_active_gateway_runs(self, false);
        self.sync_tray();
    }

    fn on_fleet_attention(&mut self, change: crate::model::fleet_attention::Change) {
        use crate::model::fleet_attention::notice_key;
        let computer = change.computer;
        if let Some(previous) = self.fleet_pending.get(&computer) {
            for prompt in previous {
                if !change.pending.iter().any(|p| p.asid == prompt.asid) {
                    let key = notice_key(&computer, &prompt.asid);
                    crate::platform::notify::withdraw(&key);
                    self.toasts
                        .retain(|t| t.target.as_deref() != Some(key.as_str()));
                }
            }
        }
        self.fleet_pending.insert(computer.clone(), change.pending);
        for attention in change.raised {
            let on_screen = self.current_computer() == computer
                && crate::platform::notify::window_focused()
                && self.mode == Mode::Session
                && (self.selected.as_deref() == Some(attention.asid.as_str())
                    || self.tile_of(&attention.asid).is_some());
            if !self.cfg.notifications.enabled || on_screen {
                continue;
            }
            let info = change.info.get(&attention.asid);
            let agent = info.map(|i| self.agent_name(&i.agent_id));
            if let Some(mut notice) = crate::model::notify::compose(
                &attention,
                info,
                agent.as_deref(),
                self.cfg.general.language,
            ) {
                notice.key = notice_key(&computer, &attention.asid);
                notice.title = format!("{} · {}", self.computer_name(&computer), notice.title);
                if notice.waiting {
                    self.toasts
                        .retain(|t| t.target.as_deref() != Some(notice.key.as_str()));
                    self.toast_seq += 1;
                    self.toasts.push(ToastItem {
                        id: self.toast_seq,
                        level: ToastLevel::Warning,
                        title: notice.title.clone(),
                        detail: Some(notice.body.clone()),
                        created_at: std::time::Instant::now(),
                        target: Some(notice.key.clone()),
                    });
                    while self.toasts.len() > MAX_TOASTS {
                        self.toasts.remove(0);
                    }
                    HAS_TOASTS.store(true, Ordering::Release);
                }
                crate::platform::notify::show(notice);
            }
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
            ThemeUpdate::SystemRefreshed(applied) => {
                if self.cfg.appearance.theme_id == crate::theme::omarchy::ID {
                    let scheme = applied.scheme;
                    if crate::theme::apply::activate(applied).is_ok() {
                        self.cfg.appearance.scheme = scheme;
                        self.dark = scheme == Scheme::Dark;
                    }
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
            self.start_model = None;
            self.start_mode = None;
            self.start_catalog = None;
            self.start_catalog_request = 0;
            self.start_catalog_failed = false;
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
        if first {
            if self.start_cwd.text.is_empty() {
                let cwd = self.start_default_directory(&self.start_agent);
                self.start_cwd = TextEdit::new(cwd.display().to_string());
            }
            if !self.start_agent.is_empty() {
                self.send(Command::LoadProjects {
                    agent: self.start_agent.clone(),
                });
                self.refresh_start_catalog();
            }
        } else if !self.start_agent.is_empty()
            && self.start_catalog.is_none()
            && self.start_catalog_request == 0
        {
            self.refresh_start_catalog();
        }
        orchestration::ensure_gateway_synced(self);
    }

    fn merge_sessions(&mut self, incoming: Vec<AgentSessionInfo>) {
        for mut info in incoming {
            if let Some(preserved) = self.orchestration.preserved_titles.get(&info.asid.0) {
                if !preserved.trim().is_empty() {
                    info.title = preserved.clone();
                }
            }
            self.remember_session_lineage(&info);
            match self.sessions.iter_mut().find(|s| s.asid.0 == info.asid.0) {
                Some(slot) => *slot = info,
                None => self.sessions.push(info),
            }
        }
        self.sessions
            .sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
        self.sessions.truncate(SESSIONS_KEPT);
    }

    pub(super) fn adopt_created_session(&mut self, asid: &str) {
        // Migrate optimistic pending session created from home composer if present.
        if let Some((pending_asid, _)) = self.pending_start.take() {
            self.sessions.retain(|s| s.asid.0 != pending_asid);
            if let Some(timeline) = self.timelines.remove(&pending_asid) {
                self.timelines.entry(asid.to_owned()).or_insert(timeline);
            }
            if let Some(pending_echoes) = self.echoes.remove(&pending_asid) {
                let list = self.echoes.entry(asid.to_owned()).or_default();
                for echo in pending_echoes {
                    if !list.iter().any(|e| e.token == echo.token) {
                        list.push(echo);
                    }
                }
            }
            if self.selected.as_deref() == Some(pending_asid.as_str()) {
                self.selected = Some(asid.to_owned());
                if self.catalog.is_none() {
                    self.catalog = self.start_catalog.clone();
                }
            }
            for tile in &mut self.tiles {
                if tile.asid.as_deref() == Some(pending_asid.as_str()) {
                    tile.asid = Some(asid.to_owned());
                    tile.content = TileContent::Session(asid.to_owned());
                }
            }
            self.sync_watch();
            self.persist_tiles();
        }
    }

    fn on_snapshot(&mut self, snapshot: muqun_gateway::agents::ports::AgentSessionSnapshot) {
        self.session_open_errors.remove(&snapshot.info.asid.0);
        let asid = snapshot.info.asid.0.clone();
        // Navigation belongs to the action that opened a pane. A delayed read can
        // update its existing pane, but cannot claim the current Home submission.
        let selected = self.selected.as_deref() == Some(&asid);
        let tracked = self.orchestration.is_tracked(&asid);
        if !selected && self.tile_of(&asid).is_none() && !tracked {
            return;
        }
        let focused = selected && self.mode == Mode::Session;
        let first_read = !self.timelines.contains_key(&asid);
        self.merge_sessions(vec![snapshot.info.clone()]);
        self.remember_timeline_lineage(&asid, &snapshot.timeline);
        let count = snapshot.timeline.len();
        self.timelines
            .entry(asid.clone())
            .or_default()
            .replace(snapshot.timeline);
        self.echo_rows(&asid);
        self.waiting_perms
            .insert(asid.clone(), snapshot.permissions);
        self.waiting_forms.insert(asid.clone(), snapshot.forms);
        let queue = InboxItem::list(&snapshot.inbox);
        self.apply_session_inbox(&asid, queue, snapshot.seq);
        if tracked {
            orchestration::on_session_status(self, &asid, snapshot.info.status);
        }
        if !focused {
            return;
        }
        self.status = format!("{count} messages");
        if self.error.as_ref().is_some_and(|e| e.unreachable) {
            self.error = None;
        }
        if first_read {
            self.session_opened(&snapshot.info);
        }
    }

    fn apply_session_inbox(&mut self, asid: &str, items: Vec<InboxItem>, seq: u64) {
        if self.selected.as_deref() != Some(asid) && self.tile_of(asid).is_none() {
            return;
        }
        if !self
            .timelines
            .entry(asid.to_string())
            .or_default()
            .apply_inbox(&items, seq)
        {
            return;
        }
        self.echo_inbox(asid, &items);
        if self.selected.as_deref() == Some(asid) {
            self.queue = items;
        } else if let Some(slot) = self.parked_slot(asid) {
            slot.queue = items;
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
                self.remember_timeline_lineage(&asid.0, &items);
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
                orchestration::on_session_status(self, &asid.0, status);
                if let Some(err) = error {
                    let message = crate::model::inbox::clean_error_message(&err.message);
                    self.error_at = Some(std::time::Instant::now());
                    self.error = Some(UiError {
                        message,
                        detail: Some(err.name),
                        unreachable: false,
                    });
                    HAS_TOASTS.store(true, Ordering::Release);
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
                match status {
                    muqun_gateway::agents::CompactionStatus::Started
                    | muqun_gateway::agents::CompactionStatus::Running => {
                        self.compacting_sessions.insert(asid.0.clone());
                    }
                    muqun_gateway::agents::CompactionStatus::Completed
                    | muqun_gateway::agents::CompactionStatus::Failed => {
                        self.compacting_sessions.remove(&asid.0);
                    }
                }
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
            AgentDomainEvent::InboxChanged { asid, items, seq } => {
                self.apply_session_inbox(&asid.0, InboxItem::list(&items), seq);
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
            UiAction::Toast { title, detail } => self.toast(title, detail),
            UiAction::ComputerUnpaired {
                computer,
                request,
                result,
            } => self.finish_computer_unpair(computer, request, result),
            UiAction::LocalPairingRevoked { id, result } => self.finish_local_revoke(id, result),
            UiAction::LocalInvite(id, result) => self.receive_local_invite(id, result),
            UiAction::DesktopRestartCheckpoint(saved) => self.restart_checkpoint_completed(saved),
            UiAction::CheckDesktopUpdateIfIdle => {
                if !matches!(
                    self.desktop_update,
                    crate::platform::update::Status::InstallFailed(_)
                ) {
                    self.check_desktop_update();
                }
            }
            UiAction::DesktopUpdate(status) => self.receive_desktop_update(status),
            UiAction::LocalPairings(devices) => {
                self.local_revoke.cancel_confirmation();
                self.local_pairings = devices;
            }
            UiAction::LocalGatewayStatus(status) => self.receive_gateway_setup(status),
            UiAction::LocalGatewayReady => {
                // Never displace a remote computer, open session, terminal or
                // pending/unfinished work when background setup completes.
                if self.can_connect_new_local_gateway() {
                    self.switch_computer("local".into());
                }
            }
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
                if self.current_computer() == "local" {
                    home::pick_folder_path(self, path.display().to_string());
                }
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
            UiAction::PluginFilePicked(path) => {
                self.plugin_source = TextEdit::new(path.to_string_lossy().as_ref())
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
            ComposerUpdate::SessionCatalogChanged(asid) => {
                if let Some(info) = self.session_by_id(&asid.0).cloned() {
                    self.request_session_catalog(&info);
                }
            }
            ComposerUpdate::Catalog {
                request,
                asid,
                agent,
                directory,
                view,
            } => {
                if let Some(asid) = asid {
                    let Some(view) = view else { return };
                    if self.selected.as_deref() == Some(asid.0.as_str()) {
                        if self.catalog_request == request {
                            self.catalog = Some(view);
                        }
                    } else if let Some(tile) = self
                        .tiles
                        .iter_mut()
                        .find(|t| t.asid.as_deref() == Some(asid.0.as_str()))
                    {
                        if tile.slot.catalog_request == request {
                            tile.slot.catalog = Some(view);
                        }
                    }
                    return;
                }
                if let Some(ref view_arc) = view {
                    orchestration::on_catalog(self, request, &agent, view_arc.clone());
                }
                let typed = self.start_cwd.text.trim();
                let start = agent == self.start_agent
                    && directory.as_deref().unwrap_or("").trim_end_matches('/')
                        == typed.trim_end_matches('/');
                if start && self.start_catalog_request == request {
                    self.start_catalog_request = 0;
                    self.start_catalog_failed = view.is_none();
                    if let Some(view) = view {
                        self.start_catalog = Some(view);
                    }
                }
            }
            ComposerUpdate::Staged { id, result } => composer::finish_staging(self, id, result),
            ComposerUpdate::Submitted { token, ok } => {
                if !ok
                    && self
                        .pending_start
                        .as_ref()
                        .is_some_and(|(_, pending_tok)| *pending_tok == token)
                {
                    self.cleanup_pending_start();
                }
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
                        if entry.token.is_none() {
                            if let Some(url) = &p.url {
                                entry.url = Some(url.clone());
                            }
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
        Look::of(&self.cfg.appearance, self.is_dark(self.dark))
    }

    pub fn scheme(&self) -> ColorScheme {
        if self.is_dark(self.dark) {
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
            &self.model_query,
            &self.task_goal,
            &self.theme_url,
            &self.plugin_source,
            &self.plugin_query,
            &self.theme_query,
            &self.cwd_input,
            &self.rename_edit,
            &self.project_edit,
            &self.quick_notes_edit,
            &self.browser.address,
        ]
        .into_iter()
        .chain(self.form_inputs.values())
        .chain(
            self.plugin_fields
                .values()
                .flat_map(|fields| fields.values()),
        )
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
        Activity::Working => crate::ui::art::ActivityMark::Compact.build(k),
        Activity::NeedsYou => look.pulse(rune::el().size(7.0, 7.0).radius(3.5).fill(color)),
        _ => rune::el().size(7.0, 7.0).radius(3.5).fill(color),
    };
    let pill = rune::row()
        .key("activity")
        .h(20.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(k.radius.lg)
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
    if s.gateway_setup.is_some() {
        return false;
    }
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
    keymap::sync_active_keymap(&s.cfg.keymap);
    // Desktop CI smoke greps this once the first frame builds (xvfb / lavapipe).
    static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    if FIRST_FRAME.swap(false, std::sync::atomic::Ordering::Relaxed) {
        tracing::debug!("build_ui");
    }
    let system_dark = cx.system_dark();
    s.system_dark.set(Some(system_dark));
    s.system_reduced_motion.set(cx.is_reduced_motion());
    let dark = s.is_dark(system_dark);
    let scheme = if dark {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    let mut k =
        crate::theme::apply::palette(scheme).map_or_else(|| Kit::neutral(dark), |p| p.kit());
    crate::theme::look::tune_kit(&mut k, &s.cfg.appearance);
    let [ring, _, _] = crate::theme::look::highlight_colors_for(&k, dark);
    cx.focus_ring(2.0, 1.5, ring.alpha(0.65));
    if s.cfg.appearance.reduced_motion {
        cx.reduced_motion(true);
    }
    let mut layer: Vec<El> = Vec::new();
    if s.gateway_setup.is_some() {
        gateway_setup::mount(s, &k, &mut layer);
        let background = rune::el().w_full().h_full().linear_gradient(
            0.8,
            [
                (0.0, k.c.bg_base),
                (0.55, k.c.bg_base.mix(k.c.icon_accent, 0.035)),
                (1.0, k.c.bg_base),
            ],
        );
        return rune_kit::root(background, layer);
    }
    let body = shell::view(s, cx, &k, &mut layer);
    viewer::image_overlay(s, &k, cx.env().scale, &mut layer);
    if let Some(selection) = s.row_menu.as_ref().and_then(|m| m.selection.as_ref()) {
        cx.mark_texts(
            selection
                .texts()
                .iter()
                .filter(|t| t.is_selected())
                .map(rune::SelectedText::mark),
        );
    }
    parts::pane::row_menu(s, &k, &mut layer);
    welcome::mount(s, &k, &mut layer);
    if let (Some(splash), Some(images)) = (s.splash.as_ref(), s.images.as_ref()) {
        if let Some(stage) = splash.view(images, &k, &s.look(), scheme) {
            layer.push(stage);
        }
    }
    palette::mount(s, &k, &mut layer);
    shortcuts::mount(s, &k, &mut layer);
    pair_computer::mount(s, &k, &mut layer);
    desktop_update::mount(s, &k, &mut layer);
    if s.error.is_some() || !s.toasts.is_empty() {
        layer.push(notifications::region(s, &k));
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
            if HAS_TOASTS.load(Ordering::Acquire) {
                handle.update(|s, _| {
                    s.expire_toasts();
                });
            }
            let batch = inbox.take();
            if batch.is_empty() {
                // A pane grip was touched: look at the widths once the
                // quiet frame arrives (a drag in progress re-arms the flag).
                if LAYOUT_DIRTY.swap(false, Ordering::AcqRel) {
                    handle.update(|s, _| s.persist_layout());
                }
                continue;
            }
            handle.update(move |s, tx| {
                let questions = |s: &UiState| -> Vec<String> {
                    s.selected
                        .as_deref()
                        .map(|asid| {
                            parts::question::pending_forms(s, asid)
                                .into_iter()
                                .map(|form| form.id.clone())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let before = questions(s);
                let previous = s.gateway_setup.clone();
                s.apply_updates(batch);
                if !s.cfg.appearance.reduced_motion
                    && !s.system_reduced_motion.get()
                    && (before != questions(s) || previous != s.gateway_setup)
                {
                    tx.animated();
                }
            });
        })
        .expect("inbox pump");
}

#[cfg(test)]
mod tests {
    fn session_fixture(id: &str) -> AgentSessionInfo {
        serde_json::from_value(serde_json::json!({
            "asid": id, "agent_id": "codex", "title": id,
            "status": "idle", "directory": "/project", "updated_ms": 1
        }))
        .unwrap()
    }

    #[test]
    fn remote_attention_routes_to_its_owner_without_leaking_local_forms_or_answers() {
        use crate::model::fleet_attention::{notice_key, Change, Pending};
        use crate::model::notify::{Attention, AttentionKind};
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".into();
        s.cfg.notifications.enabled = false;
        s.sessions = vec![session_fixture("same")];
        s.waiting_forms.insert(
            "same".into(),
            vec![serde_json::from_value(serde_json::json!({
                "id":"question", "asid":"same", "title":"local", "fields":[]
            }))
            .unwrap()],
        );
        s.form_inputs.insert(
            "question/answer".into(),
            TextEdit::new("local private draft"),
        );
        let pending = Pending {
            computer: "mini".into(),
            asid: "same".into(),
            request: "f:question".into(),
            kind: AttentionKind::Question,
            title: "remote".into(),
        };
        s.apply_updates(vec![Update::FleetAttention(Box::new(Change {
            computer: "mini".into(),
            pending: vec![pending.clone()],
            raised: vec![Attention {
                asid: "same".into(),
                kind: AttentionKind::Question,
            }],
            info: HashMap::new(),
        }))]);
        assert_eq!(s.waiting_count(), 2);
        assert_eq!(inbox::items(&s).len(), 2);
        assert!(
            s.toasts.is_empty(),
            "disabled notifications leave the inbox available without interrupting"
        );
        s.open_notification(&notice_key("mini", "same"));
        assert_eq!(s.current_computer(), "mini");
        assert_eq!(s.selected.as_deref(), Some("same"));
        assert!(s.waiting_forms.is_empty());
        assert!(s.form_inputs.is_empty());
        assert_eq!(
            s.waiting_count(),
            1,
            "the pending summary remains visible until the full form loads"
        );
        s.apply_updates(vec![Update::FleetAttention(Box::new(Change {
            computer: "mini".into(),
            pending: vec![],
            raised: vec![],
            info: HashMap::new(),
        }))]);
        assert_eq!(s.waiting_count(), 0);
        s.switch_computer("local".into());
        assert_eq!(s.form_inputs["question/answer"].text, "local private draft");
    }

    #[test]
    fn message_fork_requires_explicit_cutoff_support_for_that_sessions_agent() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap();
        scene(&mut s);
        let asid = s.selected.clone().unwrap();
        let agent_id = s.session_by_id(&asid).unwrap().agent_id.clone();
        let index = s
            .agents
            .iter()
            .position(|agent| agent.id == agent_id)
            .unwrap();
        s.agents[index]
            .features
            .extra
            .remove("sessionForkFromMessage");
        s.agents[index]
            .features
            .extra
            .insert("sessionFork".into(), serde_json::json!(true));
        assert!(
            !s.can_fork_from_message(&asid),
            "whole-session support is insufficient"
        );
        s.agents[index]
            .features
            .extra
            .insert("sessionForkFromMessage".into(), serde_json::json!(true));
        assert!(s.can_fork_from_message(&asid));
        assert!(!s.can_fork_from_message("missing-session"));
        s.agents[index]
            .features
            .extra
            .insert("sessionFork".into(), serde_json::json!(false));
        assert!(!s.can_fork_from_message(&asid));
    }

    #[test]
    fn event_only_question_is_available_and_clears_after_remote_answer() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let request: FormRequest = serde_json::from_value(serde_json::json!({
            "id":"question", "asid":"session", "title":"Question", "nonblocking":true,
            "fields":[{"type":"string", "key":"answer", "title":"Answer", "required":true}]
        }))
        .unwrap();
        s.apply_updates(vec![Update::event(AgentDomainEvent::FormPending {
            asid: AgentSessionId("session".into()),
            request,
            seq: 1,
        })]);
        assert_eq!(parts::question::pending_forms(&s, "session").len(), 1);
        let mut next = s.waiting_forms["session"][0].clone();
        next.id = "next-question".into();
        s.apply_updates(vec![Update::event(AgentDomainEvent::FormPending {
            asid: AgentSessionId("session".into()),
            request: next,
            seq: 2,
        })]);
        s.form_inputs
            .insert("next-question/answer".into(), TextEdit::new("keep me"));
        assert_eq!(parts::question::pending_forms(&s, "session").len(), 2);
        assert!(parts::question::pending_forms(&s, "other").is_empty());
        s.form_inputs
            .insert("question/answer".into(), TextEdit::new("unfinished"));
        s.apply_updates(vec![Update::event(AgentDomainEvent::FormResolved {
            asid: AgentSessionId("session".into()),
            form_id: "question".into(),
            reason: None,
            seq: 3,
        })]);
        let remaining = parts::question::pending_forms(&s, "session");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "next-question");
        assert_eq!(s.form_inputs["next-question/answer"].text, "keep me");
        assert!(!s.form_inputs.contains_key("question/answer"));
    }

    #[test]
    fn delayed_catalog_returns_to_requesting_tile() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.selected = Some("focused".into());
        s.tiles = vec![Tile::session("focused"), Tile::session("parked")];
        let current = Arc::new(CatalogView {
            catalog: Default::default(),
        });
        let arriving = Arc::new(CatalogView {
            catalog: Default::default(),
        });
        s.catalog = Some(current.clone());
        s.tiles[1].slot.catalog_request = 2;
        s.on_composer(ComposerUpdate::Catalog {
            request: 2,
            asid: Some(AgentSessionId("parked".into())),
            agent: "codex".into(),
            directory: None,
            view: Some(arriving.clone()),
        });
        // A slower read started before the model switch completes afterwards.
        s.on_composer(ComposerUpdate::Catalog {
            request: 1,
            asid: Some(AgentSessionId("parked".into())),
            agent: "codex".into(),
            directory: None,
            view: Some(current.clone()),
        });
        assert!(Arc::ptr_eq(s.catalog.as_ref().unwrap(), &current));
        assert!(Arc::ptr_eq(
            s.tiles[1].slot.catalog.as_ref().unwrap(),
            &arriving
        ));
        s.focus_tile(1);
        assert!(Arc::ptr_eq(s.catalog.as_ref().unwrap(), &arriving));
    }

    #[test]
    fn aggregate_click_routes_before_open_and_restores_local_draft() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.demo = false;
        s.cfg.current_computer = "self".into();
        s.sessions = vec![session_fixture("local-session")];
        s.open_session("local-session");
        s.draft = TextEdit::new("unfinished local draft");
        s.computer_data.insert(
            "mini".into(),
            ComputerData {
                sessions: vec![session_fixture("remote-session")],
                ..Default::default()
            },
        );
        s.switch_computer("all".into());
        while rx.try_recv().is_ok() {}
        s.open_session("remote-session");
        assert_eq!(s.current_computer(), "mini");
        let mut switched = false;
        let mut opened = false;
        while let Ok(command) = rx.try_recv() {
            match command {
                Command::SwitchComputer(computer) => {
                    assert_eq!(computer, "mini");
                    switched = true;
                }
                Command::OpenSession(asid) | Command::OpenTile(asid) => {
                    assert!(switched, "must select the owning hub before opening");
                    assert_eq!(asid.0, "remote-session");
                    opened = true;
                }
                _ => {}
            }
        }
        assert!(opened);
        s.switch_computer("all".into());
        s.open_session("local-session");
        assert_eq!(s.current_computer(), "local");
        assert_eq!(s.draft.text, "unfinished local draft");
        assert!(!s.computer_data.contains_key("all"));
        assert!(!s.computer_data.contains_key("self"));
    }

    #[test]
    fn background_computer_updates_do_not_cross_workspaces_and_delivery_settles_its_draft() {
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.demo = false;
        s.cfg.current_computer = "local".into();
        s.sessions = vec![session_fixture("same-id")];
        s.open_session("same-id");
        s.draft = TextEdit::new("local submitted draft");
        let token = s.token(Slot::Session);
        s.switch_computer("mini".into());
        s.sessions = vec![session_fixture("same-id")];
        s.open_session("same-id");
        s.draft = TextEdit::new("remote unsent draft");
        s.apply_updates(vec![Update::Computer {
            computer: "local".into(),
            generation: 0,
            updates: vec![
                Update::SessionOpenFailed {
                    asid: "same-id".into(),
                    message: "old error".into(),
                },
                Update::Composer(Box::new(ComposerUpdate::Submitted { token, ok: true })),
            ],
        }]);
        assert!(s.session_open_errors.is_empty());
        assert_eq!(s.draft.text, "remote unsent draft");
        s.switch_computer("local".into());
        assert!(s.pending_submit.is_none());
        assert!(
            s.draft.text.is_empty(),
            "successful old delivery must settle the local draft"
        );
        s.switch_computer("mini".into());
        assert_eq!(s.draft.text, "remote unsent draft");
    }

    #[test]
    fn created_session_receipt_survives_leaving_its_computer() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.demo = false;
        s.cfg.current_computer = "local".into();
        s.sessions = vec![session_fixture("new-42")];
        s.tiles = vec![Tile::session("new-42")];
        s.selected = Some("new-42".into());
        s.pending_start = Some(("new-42".into(), 42));
        s.pending_submit = Some((42, Slot::Start));
        s.start_draft = TextEdit::new("sent to local");
        s.switch_computer("mini".into());
        s.start_draft = TextEdit::new("keep remote draft");
        s.apply_updates(vec![Update::Computer {
            computer: "local".into(),
            generation: 0,
            updates: vec![
                Update::SessionCreated(Box::new(session_fixture("created-local")), Some(42)),
                Update::Composer(Box::new(ComposerUpdate::Submitted {
                    token: 42,
                    ok: true,
                })),
            ],
        }]);
        assert_eq!(s.start_draft.text, "keep remote draft");
        while rx.try_recv().is_ok() {}
        s.switch_computer("local".into());
        assert_eq!(s.selected.as_deref(), Some("created-local"));
        assert!(s.pending_start.is_none());
        assert!(s.pending_submit.is_none());
        assert!(s.sessions.iter().all(|session| session.asid.0 != "new-42"));
        assert!(s.start_draft.text.is_empty());
        let mut reopened = false;
        let mut switched = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(&command, Command::SwitchComputer(computer) if computer == "local") {
                switched = true;
            }
            if let Command::OpenTile(asid) = command {
                assert!(
                    switched,
                    "receipt-triggered reads must follow the backend barrier"
                );
                assert_eq!(asid.0, "created-local");
                reopened = true;
            }
        }
        assert!(reopened);
        s.switch_computer("mini".into());
        assert_eq!(s.start_draft.text, "keep remote draft");
    }

    #[test]
    fn aggregate_lists_keep_origin_and_retry_reopens_an_existing_tile() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.demo = false;
        s.cfg.current_computer = "local".into();
        s.sessions = vec![session_fixture("local-session")];
        s.switch_computer("all".into());
        s.apply_updates(vec![Update::Computer {
            computer: "mini".into(),
            generation: 4,
            updates: vec![Update::AgentSessions {
                agent: "codex".into(),
                sessions: vec![session_fixture("remote-session")],
            }],
        }]);
        assert_eq!(
            s.computer_of_session("remote-session").as_deref(),
            Some("mini")
        );
        assert_eq!(
            s.computer_of_session("local-session").as_deref(),
            Some("local")
        );
        s.open_session("remote-session");
        s.apply_updates(vec![Update::SessionOpenFailed {
            asid: "remote-session".into(),
            message: "failed".into(),
        }]);
        assert!(s.session_open_errors.contains_key("remote-session"));
        while rx.try_recv().is_ok() {}
        s.retry_open_session("remote-session");
        assert!(!s.session_open_errors.contains_key("remote-session"));
        assert!(matches!(rx.try_recv(), Ok(Command::OpenSession(id)) if id.0 == "remote-session"));
        s.apply_updates(vec![Update::Computer {
            computer: "mini".into(),
            generation: 3,
            updates: vec![Update::SessionOpenFailed {
                asid: "remote-session".into(),
                message: "stale failure".into(),
            }],
        }]);
        assert!(s.session_open_errors.is_empty());
    }

    #[test]
    fn restored_split_loads_every_session_without_focus_clicks() {
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = super::UiState::new(tx);
        s.demo = false;
        s.tiles_restored = false;
        s.cfg.ui.tiles = vec!["session:left".into(), "session:right".into()];
        s.sessions = ["left", "right"]
            .into_iter()
            .map(|id| {
                serde_json::from_value(serde_json::json!({
                    "asid": id, "agent_id": "codex", "title": id,
                    "status": "idle", "directory": "/project", "updated_ms": 1
                }))
                .unwrap()
            })
            .collect();
        s.restore_tiles();
        let mut opened = std::collections::HashSet::new();
        while let Ok(command) = rx.try_recv() {
            if let crate::model::commands::Command::OpenSession(id)
            | crate::model::commands::Command::OpenTile(id) = command
            {
                opened.insert(id.0);
            }
        }
        assert_eq!(
            opened,
            ["left".to_string(), "right".to_string()]
                .into_iter()
                .collect()
        );
        assert_eq!(s.selected.as_deref(), Some("left"));
    }
    use super::*;

    #[test]
    fn toasts_expire_after_ttl() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = UiState::new(tx);
        s.toast("Notice", None);
        assert_eq!(s.toasts.len(), 1);
        assert!(HAS_TOASTS.load(Ordering::Acquire));

        // Not yet expired
        assert!(!s.expire_toasts());
        assert_eq!(s.toasts.len(), 1);

        // Move created_at into the past
        s.toasts[0].created_at = std::time::Instant::now() - std::time::Duration::from_millis(3600);
        assert!(s.expire_toasts());
        assert!(s.toasts.is_empty());
        assert!(!HAS_TOASTS.load(Ordering::Acquire));
    }

    #[test]
    fn on_agents_triggers_start_catalog_refresh_on_first_launch() {
        use crate::model::commands::Command;
        use crate::model::compose::ComposeCommand;
        use muqun_gateway::platform::discovery::{
            AgentAvailability, AgentDiscoveryInfo, AgentFeatures,
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = UiState::new(tx);
        s.start_cwd = TextEdit::new(""); // simulate empty cwd on cold start
        s.start_agent = "claude".into();

        let agent = AgentDiscoveryInfo {
            id: "claude".into(),
            name: "Claude Code".into(),
            kind: "claude".into(),
            status: AgentAvailability::Connected,
            enabled: true,
            endpoint: None,
            version: None,
            reason: None,
            models: Vec::new(),
            modes: Vec::new(),
            features: AgentFeatures {
                streaming: true,
                reasoning_effort: false,
                model_selection: true,
                tool_approvals: false,
                worktrees: false,
                revert: false,
                staged_revert: false,
                inbox: false,
                modes: true,
                skills: false,
                slash_commands: false,
                compaction: false,
                background_shells: false,
                attachments: false,
                attachments_by_path: false,
                extra: Default::default(),
            },
        };

        s.on_agents(vec![agent]);

        // Verify that commands were emitted:
        // 1. LoadProjects
        // 2. ComposeCommand::Catalog
        let mut saw_load_projects = false;
        let mut saw_catalog = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::LoadProjects { agent } => {
                    assert_eq!(agent, "claude");
                    saw_load_projects = true;
                }
                Command::Compose(ComposeCommand::Catalog { agent, .. }) => {
                    assert_eq!(agent, "claude");
                    saw_catalog = true;
                }
                _ => {}
            }
        }
        assert!(saw_load_projects, "expected LoadProjects on first launch");
        assert!(
            saw_catalog,
            "expected Catalog refresh on first launch even when start_cwd was empty"
        );
    }

    #[test]
    fn clean_error_message_strips_nested_agent_request_failed_prefixes() {
        use crate::model::inbox::clean_error_message;
        assert_eq!(
            clean_error_message(
                "Agent request failed: Agent request failed: Cursor ACP request rejected (code -32602): Invalid params"
            ),
            "Cursor ACP request rejected (code -32602): Invalid params"
        );
        assert_eq!(
            clean_error_message("Agent request failed: timeout"),
            "timeout"
        );
        assert_eq!(clean_error_message("Plain error"), "Plain error");
    }

    #[test]
    fn late_session_reads_cannot_claim_a_new_home_submission_or_steal_navigation() {
        use muqun_gateway::agents::ports::AgentSessionSnapshot;
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.demo = false;
        s.cfg.current_computer = "local".into();
        s.open_session("previous");
        s.go(Mode::Home);
        home::pick_folder_path(&mut s, "/work/picked".into());
        home::pick_agent(&mut s, "copilot".into());
        s.switch_computer("mini".into());
        assert_eq!(s.start_cwd.text, "~");
        home::pick_folder_path(&mut s, "/remote/project".into());
        s.switch_computer("local".into());
        assert_eq!(s.start_cwd.text, "/work/picked");
        s.start_draft = TextEdit::new("work here");
        s.splash = None;
        let mut rt = rune::Runtime::new(s, view, (1440.0, 900.0), rune::Env::new(true, 1.0));
        for frame in 0..4 {
            rt.frame(frame as f64 / 60.0);
        }
        let (x, y) = rt.find("start").expect("Home start button");
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
        for frame in 4..12 {
            rt.frame(frame as f64 / 60.0);
        }
        let (_, token) = rt.state.pending_start.as_ref().expect("submit started");
        assert!(
            rt.find(&format!("echo-{token}")).is_some(),
            "the first prompt is painted before the backend replies"
        );
        let s = &mut rt.state;
        let (pending, token) = s.pending_start.clone().unwrap();
        assert_eq!(
            s.current().unwrap().directory.as_deref(),
            Some("/work/picked")
        );
        assert!(s.timelines.contains_key(&pending));
        let snapshot = |id| {
            Update::Snapshot(Box::new(AgentSessionSnapshot {
                info: session_fixture(id),
                timeline: vec![],
                permissions: vec![],
                forms: vec![],
                inbox: vec![],
                seq: 1,
            }))
        };
        s.apply_updates(vec![
            snapshot("previous"),
            Update::AgentSessions {
                agent: "copilot".into(),
                sessions: vec![],
            },
            Update::SessionCreated(Box::new(session_fixture("other")), Some(token + 1)),
        ]);
        assert_eq!(s.current().unwrap().asid.0, pending);
        assert_eq!(s.echoes_of(&pending).len(), 1);
        let mut sent_to = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            if let Command::Compose(crate::model::compose::ComposeCommand::Submit {
                target: crate::model::compose::Target::New { cwd, .. },
                ..
            }) = cmd
            {
                sent_to.push(cwd);
            }
        }
        assert_eq!(sent_to, [PathBuf::from("/work/picked")]);
        s.go(Mode::Home);
        s.apply_updates(vec![
            Update::SessionCreated(Box::new(session_fixture("created")), Some(token)),
            snapshot("created"),
            snapshot("previous"),
        ]);
        assert_eq!(s.mode, Mode::Home);
        assert_eq!(s.selected.as_deref(), Some("created"));
        assert_eq!(s.echoes_of("created").len(), 1);
        assert!(s.echoes_of("previous").is_empty());
    }

    #[test]
    fn submit_start_does_not_open_optimistic_placeholder_on_backend_and_cleans_up_on_failure() {
        use crate::model::commands::Command;
        use crate::model::compose::{ComposeCommand, ComposerUpdate, Target};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = UiState::new(tx);
        s.start_agent = "cursor".into();
        s.start_cwd = TextEdit::new("/tmp");
        s.start_draft = TextEdit::new("Hello Cursor");

        home::submit_start(&mut s);

        assert_eq!(s.selected.as_deref(), Some("new-1"));
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(s.sessions[0].asid.0, "new-1");

        let mut saw_submit = false;
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::OpenSession(asid) => {
                    panic!("should not send OpenSession for optimistic placeholder {asid:?}");
                }
                Command::OpenTile(asid) => {
                    panic!("should not send OpenTile for optimistic placeholder {asid:?}");
                }
                Command::FocusSession(asid) => {
                    panic!("should not send FocusSession for optimistic placeholder {asid:?}");
                }
                Command::Compose(ComposeCommand::Submit {
                    token,
                    target:
                        Target::New {
                            model, mode, agent, ..
                        },
                    ..
                }) => {
                    assert_eq!(token, 1);
                    assert_eq!(agent, "cursor");
                    assert!(model.is_none());
                    assert!(mode.is_none());
                    saw_submit = true;
                }
                _ => {}
            }
        }
        assert!(saw_submit, "expected Submit command");

        // Simulate failed submission: optimistic session should be cleaned up and draft restored.
        s.apply_updates(vec![Update::Composer(Box::new(
            ComposerUpdate::Submitted {
                token: 1,
                ok: false,
            },
        ))]);
        assert!(s.sessions.is_empty());
        assert!(s.pending_start.is_none());
        assert_eq!(s.mode, Mode::Home);
        assert_eq!(s.start_draft.text, "Hello Cursor");
    }

    #[test]
    fn newly_installed_gateway_connects_only_from_an_idle_local_view() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = UiState::new(tx);
        s.cfg.current_computer = "local".into();
        s.cfg.general.gateway_local_only = false;
        s.selected = None;
        s.tiles.clear();
        s.busy_since.clear();
        s.pending_start = None;
        s.pending_submit = None;
        s.term.open = false;
        s.start_draft = TextEdit::new("");
        s.on_action(UiAction::LocalGatewayReady);
        let mut connected = false;
        while let Ok(command) = rx.try_recv() {
            connected |=
                matches!(command, Command::SwitchComputer(ref target) if target == "local");
        }
        assert!(connected);
        s.cfg.current_computer = "work-mac".into();
        s.on_action(UiAction::LocalGatewayReady);
        assert!(rx.try_recv().is_err());
        s.cfg.current_computer = "local".into();
        s.start_draft = TextEdit::new("unfinished local work");
        s.on_action(UiAction::LocalGatewayReady);
        assert!(rx.try_recv().is_err());
        assert_eq!(s.start_draft.text, "unfinished local work");
        s.start_draft = TextEdit::new("");
        s.selected = Some("active-session".into());
        s.on_action(UiAction::LocalGatewayReady);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn toasts_deduplicate_and_errors_auto_expire() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut s = UiState::new(tx);
        s.toast("Saved", Some("/tmp/a.rs".into()));
        s.toast("Saved", Some("/tmp/a.rs".into()));
        assert_eq!(s.toasts.len(), 1);

        s.apply_updates(vec![Update::Error(UiError {
            message: "Agent request failed: Agent request failed: Boom".into(),
            detail: None,
            unreachable: false,
        })]);
        assert_eq!(s.error.as_ref().unwrap().message, "Boom");
        assert!(!s.expire_toasts());

        s.error_at = Some(std::time::Instant::now() - std::time::Duration::from_millis(6100));
        assert!(s.expire_toasts());
        assert!(s.error.is_none());
    }
}
