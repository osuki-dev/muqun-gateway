//! What the composer remembers while it is on screen: the start page's
//! choices, each folder's catalogue, the files an `@` found, the staged
//! attachments, the prompts waiting behind a busy agent, the open popup and
//! menu. All of it is keyed Blinc state written on the main thread -- by the
//! composer's own handlers, or by [`apply_update`] for what the core sent.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use blinc_core::{BlincContextState, State};
use blinc_layout::widgets::text_input::{text_input_data_with_placeholder, SharedTextInputData};
use muqun_gateway::agents::ModelRef;

use super::triggers::Trigger;
use crate::hub::{CatalogView, PeerMention};
use crate::model::compose::{BranchInfo, ComposerUpdate, FileHit, InboxItem, StagedFile};

/// What the start page will create, as far as the owner has said. Anything
/// `None` is resolved from the defaults ([`super::resolve`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartChoice {
    /// The folder as chosen (`~` expanded).
    pub project: Option<String>,
    pub agent: Option<String>,
    pub model: Option<ModelRef>,
    pub mode: Option<String>,
    /// Begin in a new git worktree of the chosen project (start page).
    pub worktree: bool,
}

/// The catalogue of each (agent, folder) the composer has asked about.
#[derive(Debug, Clone, Default)]
pub struct Catalogs {
    pub loaded: HashMap<String, Arc<CatalogView>>,
}

pub fn catalog_key(agent: &str, directory: Option<&str>) -> String {
    format!("{agent}\u{1f}{}", directory.unwrap_or(""))
}

/// The files an `@` search found, for the generation that asked.
#[derive(Debug, Clone, Default)]
pub struct FileState {
    pub hits: Vec<FileHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedStatus {
    Uploading,
    Ready(StagedFile),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub id: u64,
    pub name: String,
    pub status: StagedStatus,
}

/// The `@` / `/` / `$` list, when the caret is in a trigger word.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Popup {
    pub trigger: Option<Trigger>,
    pub selected: usize,
    /// Esc closed the list for the word starting here; it stays closed until
    /// the caret is in some other trigger.
    pub dismissed_at: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuId {
    Project,
    Agent,
    Model,
    Mode,
    Effort,
    Branch,
    Usage,
    Yolo,
}

#[derive(Clone)]
pub struct Cmp {
    pub start: State<Arc<StartChoice>>,
    pub catalogs: State<Arc<Catalogs>>,
    pub files: State<Arc<FileState>>,
    /// Computers `@` may name (this machine + linked peers when available).
    pub peers: State<Arc<Vec<PeerMention>>>,
    pub staged: State<Arc<Vec<Staged>>>,
    pub inbox: State<Arc<Vec<InboxItem>>>,
    pub popup: State<Arc<Popup>>,
    pub menu: State<Option<MenuId>>,
    /// Keyboard focus among the open menu's enabled rows.
    pub menu_cursor: State<usize>,
    /// Live filter text for the open model (or branch) menu.
    pub menu_query: State<String>,
    /// The model menu's search field.
    pub model_filter: SharedTextInputData,
    /// The overlay showing it.
    pub menu_handle: State<Option<u64>>,
    /// The overlay showing the `@` / `/` / `$` list.
    pub popup_handle: State<Option<u64>>,
    /// How many rows that overlay was opened for.
    pub popup_shown: State<usize>,
    /// The token of the submit in flight.
    pub submitting: State<Option<u64>>,
    /// The text area's height in rows (2 to 6).
    pub rows: State<usize>,
    pub branches: State<Arc<HashMap<String, BranchInfo>>>,
    /// Per session: does the agent run in its bypass mode (YOLO)? Absent
    /// until the gateway has answered.
    pub yolo: State<Arc<HashMap<String, bool>>>,
    /// A files drag is over the window (the card's drop highlight).
    pub drop_hover: State<bool>,
}

impl Cmp {
    pub fn get() -> Self {
        let ctx = BlincContextState::get();
        Self {
            start: ctx.use_state_keyed("mq.cmp.start", || Arc::new(StartChoice::default())),
            catalogs: ctx.use_state_keyed("mq.cmp.catalogs", || Arc::new(Catalogs::default())),
            files: ctx.use_state_keyed("mq.cmp.files", || Arc::new(FileState::default())),
            peers: ctx.use_state_keyed("mq.cmp.peers", || Arc::new(Vec::new())),
            staged: ctx.use_state_keyed("mq.cmp.staged", || Arc::new(Vec::new())),
            inbox: ctx.use_state_keyed("mq.cmp.inbox", || Arc::new(Vec::new())),
            popup: ctx.use_state_keyed("mq.cmp.popup", || Arc::new(Popup::default())),
            menu: ctx.use_state_keyed("mq.cmp.menu", || None),
            menu_cursor: ctx.use_state_keyed("mq.cmp.menu_cursor", || 0usize),
            menu_query: ctx.use_state_keyed("mq.cmp.menu_query", String::new),
            model_filter: ctx
                .use_state_keyed("mq.cmp.model_filter", || {
                    text_input_data_with_placeholder("Search models")
                })
                .get(),
            menu_handle: ctx.use_state_keyed("mq.cmp.menu_handle", || None),
            popup_handle: ctx.use_state_keyed("mq.cmp.popup_handle", || None),
            popup_shown: ctx.use_state_keyed("mq.cmp.popup_shown", || 0usize),
            submitting: ctx.use_state_keyed("mq.cmp.submitting", || None),
            rows: ctx.use_state_keyed("mq.cmp.rows", || 2usize),
            branches: ctx.use_state_keyed("mq.cmp.branches", || Arc::new(HashMap::new())),
            yolo: ctx.use_state_keyed("mq.cmp.yolo", || Arc::new(HashMap::new())),
            drop_hover: ctx.use_state_keyed("mq.cmp.drop_hover", || false),
        }
    }
}

/// Clear the open menu's typeahead / search field.
pub fn clear_menu_query() {
    let cmp = Cmp::get();
    if !cmp.menu_query.get().is_empty() {
        cmp.menu_query.signal().set(String::new());
    }
    let filter = cmp.model_filter.clone();
    if let Ok(mut data) = filter.lock() {
        if !data.value.is_empty() {
            data.value.clear();
            data.cursor = 0;
        }
    };
}

/// Main-thread bookkeeping that is not rendered.
#[derive(Default)]
struct Local {
    /// The draft key the text area currently holds.
    surface: Option<String>,
    /// The last text written to the config, to skip repeats.
    saved: Option<(String, String)>,
    /// What each surface last sent, for ↑ in an empty box.
    last_sent: HashMap<String, String>,
    /// What was asked for, and when (catalogues, branches, projects).
    asked: HashMap<String, std::time::Instant>,
    next_id: u64,
    generation: u64,
    /// The asid whose inbox `Cmp::inbox` holds.
    inbox_of: Option<String>,
    /// When the menu last closed itself on an outside press: the press that
    /// closed it must not also reopen it through the pill's click.
    menu_closed_at: Option<(MenuId, std::time::Instant)>,
    /// A YOLO switch has been asked of the gateway and not yet answered.
    yolo_switching: bool,
}

thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local::default());
}

fn with<R>(f: impl FnOnce(&mut Local) -> R) -> R {
    LOCAL.with(|l| f(&mut l.borrow_mut()))
}

pub fn next_id() -> u64 {
    with(|l| {
        l.next_id += 1;
        l.next_id
    })
}

pub fn next_generation() -> u64 {
    with(|l| {
        l.generation += 1;
        l.generation
    })
}

pub fn current_surface() -> Option<String> {
    with(|l| l.surface.clone())
}

pub fn set_surface(key: &str) {
    with(|l| l.surface = Some(key.to_string()));
}

/// Whether `key` should be asked for now: never asked, or asked more than
/// `retry` ago without an answer (`answered` false) -- an agent that was not
/// attached yet answers later.
pub fn ask_due(key: &str, answered: bool, retry: std::time::Duration) -> bool {
    with(|l| {
        let now = std::time::Instant::now();
        let due = match l.asked.get(key) {
            None => true,
            Some(at) => !answered && now.duration_since(*at) >= retry,
        };
        if due {
            l.asked.insert(key.to_string(), now);
        }
        due
    })
}

/// Forget that `key` was asked, so the next [`ask_due`] fires at once.
pub fn forget_asked(key: &str) {
    with(|l| {
        l.asked.remove(key);
    });
}

/// Claim the one YOLO switch that may be in flight; false when one is.
pub fn begin_yolo_switch() -> bool {
    with(|l| !std::mem::replace(&mut l.yolo_switching, true))
}

pub fn draft_changed(key: &str, text: &str) -> bool {
    with(|l| {
        let same = l.saved.as_ref().is_some_and(|(k, t)| k == key && t == text);
        if !same {
            l.saved = Some((key.to_string(), text.to_string()));
        }
        !same
    })
}

pub fn remember_sent(key: &str, text: &str) {
    with(|l| {
        l.last_sent.insert(key.to_string(), text.to_string());
    });
}

pub fn last_sent(key: &str) -> Option<String> {
    with(|l| l.last_sent.get(key).cloned())
}

pub fn mark_menu_closed(id: MenuId) {
    with(|l| l.menu_closed_at = Some((id, std::time::Instant::now())));
}

/// Whether `id`'s menu was just closed by the press that is now clicking
/// its pill.
pub fn menu_just_closed(id: MenuId) -> bool {
    with(|l| {
        l.menu_closed_at
            .is_some_and(|(m, at)| m == id && at.elapsed() < std::time::Duration::from_millis(250))
    })
}

/// What the core sent.
pub fn apply_update(update: ComposerUpdate) {
    let cmp = Cmp::get();
    match update {
        ComposerUpdate::Files { hits } => {
            cmp.files.signal().set(Arc::new(FileState { hits }));
            super::sync_popup();
        }
        ComposerUpdate::Catalog {
            agent,
            directory,
            view,
        } => {
            let mut next = (*cmp.catalogs.get()).clone();
            next.loaded
                .insert(catalog_key(&agent, directory.as_deref()), view);
            cmp.catalogs.signal().set(Arc::new(next));
            super::sync_popup();
        }
        ComposerUpdate::Staged { id, result } => {
            let mut list = (*cmp.staged.get()).clone();
            if let Some(entry) = list.iter_mut().find(|s| s.id == id) {
                entry.status = match result {
                    Ok(file) => StagedStatus::Ready(file),
                    Err(why) => StagedStatus::Failed(why),
                };
                cmp.staged.signal().set(Arc::new(list));
            }
        }
        ComposerUpdate::Submitted { token, ok } => {
            if cmp.submitting.get() == Some(token) {
                cmp.submitting.signal().set(None);
                if ok {
                    super::after_submit_ok();
                } else {
                    super::restore_after_failure();
                }
            }
        }
        ComposerUpdate::Inbox { asid, items } => {
            with(|l| l.inbox_of = Some(asid));
            cmp.inbox.signal().set(Arc::new(items));
        }
        ComposerUpdate::Branch { cwd, info } => {
            let mut next = (*cmp.branches.get()).clone();
            next.insert(cwd, info);
            cmp.branches.signal().set(Arc::new(next));
        }
        ComposerUpdate::AutoApprove { asid, enabled } => {
            with(|l| l.yolo_switching = false);
            if let Some(on) = enabled {
                if cmp.yolo.get().get(&asid) != Some(&on) {
                    let mut next = (*cmp.yolo.get()).clone();
                    next.insert(asid, on);
                    cmp.yolo.signal().set(Arc::new(next));
                }
            }
        }
        ComposerUpdate::Peers { peers } => {
            cmp.peers.signal().set(Arc::new(peers));
            super::sync_popup();
        }
    }
}

/// The inbox only shows for the session it belongs to: clear it when the
/// composer is on another surface.
pub fn inbox_of() -> Option<String> {
    with(|l| l.inbox_of.clone())
}

pub fn clear_inbox() {
    with(|l| l.inbox_of = None);
    let cmp = Cmp::get();
    if !cmp.inbox.get().is_empty() {
        cmp.inbox.signal().set(Arc::new(Vec::new()));
    }
}
