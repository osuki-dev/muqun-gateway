//! Every keyboard shortcut the window answers to, in one table.
//!
//! [`ENTRIES`] is the single source of truth: [`dispatch`] (the window's
//! `on_key`) runs what it binds, the shortcuts sheet and Settings →
//! Shortcuts list it, the palette shows each command's chord on the right and
//! buttons name theirs in their tooltips ([`tip`]). Chords are Rune's
//! ([`rune::Keybind`]): `mod` is ⌘ on macOS and Ctrl elsewhere, and Rune
//! resolves and draws them per platform.
//!
//! Who hears a key first is Rune's routing: a focused text field keeps its
//! editing keys (characters, Enter, End, arrows, its edit commands) and
//! passes the rest here, so `Ctrl+B/F/L/P/N` work while typing and `j`, `?`
//! or End only when no field has the keyboard. The terminal takes every
//! key; on Windows and Linux the app's Ctrl chords reach the window there
//! with Shift added (`term::input::route`), on macOS ⌘ chords always do.

use std::sync::OnceLock;

use rune::{KeyEvent, KeyPlatform, Keybind, Tx};

use super::{Mode, OpenMenu, UiState};
use crate::config::{KeymapConfig, Language};

/// Which shortcut is currently being recorded in `Settings → Shortcuts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingTarget {
    Leader,
    Action(Action),
}

thread_local! {
    static ACTIVE_KEYMAP: std::cell::RefCell<KeymapConfig> =
        std::cell::RefCell::new(KeymapConfig::default());
}

/// Sync the active keymap snapshot with `cfg` so `label`, `keycaps_on`,
/// `clashes`, and `terminal_shadows` reflect user customizations everywhere.
pub fn sync_active_keymap(cfg: &KeymapConfig) {
    ACTIVE_KEYMAP.with(|cell| {
        if *cell.borrow() != *cfg {
            *cell.borrow_mut() = cfg.clone();
        }
    });
}

/// Return a snapshot of the active `KeymapConfig`.
#[allow(dead_code)]
pub fn active_keymap() -> KeymapConfig {
    ACTIVE_KEYMAP.with(|cell| cell.borrow().clone())
}

/// Safely parse a Rune keybind spec (normalizing `+,` to `+comma`), returning
/// `None` if empty or malformed instead of panicking.
pub fn try_parse_keybind(spec: &str) -> Option<Keybind> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = if trimmed == "," {
        "comma".to_string()
    } else {
        trimmed.replace("+,", "+comma")
    };
    let bind = std::panic::catch_unwind(|| Keybind::new(&normalized)).ok()?;
    if bind.chords_on(KeyPlatform::current()).is_empty()
        && bind.chords_on(KeyPlatform::Linux).is_empty()
        && bind.chords_on(KeyPlatform::Mac).is_empty()
    {
        return None;
    }
    Some(bind)
}

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Shortcuts,
    Palette,
    QuickOpen,
    NewSession,
    Home,
    Inbox,
    Tasks,
    Settings,
    Back,
    ToggleSidebar,
    ToggleTerminal,
    ToggleFocus,
    NewShell,
    ToggleInspector,
    ToggleGit,
    FocusSearch,
    SearchMessages,
    ToggleRecentSession,
    PrevSession,
    NextSession,
    AddSplit,
    RemoveSplit,
    FocusPane,
    FocusPaneToward,
    SwapPaneToward,
    FocusComposer,
    ExpandComposer,
    PromptHistory,
    Send,
    Queue,
    Stop,
    ModelPicker,
    Dictation,
    JumpLatest,
    NextRow,
    PrevRow,
    ToggleRow,
    OpenRow,
    NextMessage,
    PrevMessage,
    TermPrefix,
    TermNewTab,
    TermCloseTab,
    TermSwitchTab,
    TermCycleTab,
    TermSplit,
    TermSearch,
    TermCopy,
    TermPaste,
    Allow,
    AllowAlways,
    Deny,
    MarkDone,
    CancelTask,
}

impl Action {
    /// Stable identifier used as the key in `KeymapConfig::bindings`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Shortcuts => "shortcuts",
            Self::Palette => "palette",
            Self::QuickOpen => "quick_open",
            Self::NewSession => "new_session",
            Self::Home => "home",
            Self::Inbox => "inbox",
            Self::Tasks => "tasks",
            Self::Settings => "settings",
            Self::Back => "back",
            Self::ToggleSidebar => "toggle_sidebar",
            Self::ToggleTerminal => "toggle_terminal",
            Self::ToggleFocus => "toggle_focus",
            Self::NewShell => "new_shell",
            Self::ToggleInspector => "toggle_inspector",
            Self::ToggleGit => "toggle_git",
            Self::FocusSearch => "focus_search",
            Self::SearchMessages => "search_messages",
            Self::ToggleRecentSession => "toggle_recent_session",
            Self::PrevSession => "prev_session",
            Self::NextSession => "next_session",
            Self::AddSplit => "add_split",
            Self::RemoveSplit => "remove_split",
            Self::FocusPane => "focus_pane",
            Self::FocusPaneToward => "focus_pane_toward",
            Self::SwapPaneToward => "swap_pane_toward",
            Self::FocusComposer => "focus_composer",
            Self::ExpandComposer => "expand_composer",
            Self::PromptHistory => "prompt_history",
            Self::Send => "send",
            Self::Queue => "queue",
            Self::Stop => "stop",
            Self::ModelPicker => "model_picker",
            Self::Dictation => "dictation",
            Self::JumpLatest => "jump_latest",
            Self::NextRow => "next_row",
            Self::PrevRow => "prev_row",
            Self::ToggleRow => "toggle_row",
            Self::OpenRow => "open_row",
            Self::NextMessage => "next_message",
            Self::PrevMessage => "prev_message",
            Self::TermPrefix => "term_prefix",
            Self::TermNewTab => "term_new_tab",
            Self::TermCloseTab => "term_close_tab",
            Self::TermSwitchTab => "term_switch_tab",
            Self::TermCycleTab => "term_cycle_tab",
            Self::TermSplit => "term_split",
            Self::TermSearch => "term_search",
            Self::TermCopy => "term_copy",
            Self::TermPaste => "term_paste",
            Self::Allow => "allow",
            Self::AllowAlways => "allow_always",
            Self::Deny => "deny",
            Self::MarkDone => "mark_done",
            Self::CancelTask => "cancel_task",
        }
    }

    /// Resolve an `Action` from its stable identifier.
    pub fn from_id(id: &str) -> Option<Self> {
        ENTRIES
            .iter()
            .find(|e| e.action.id() == id)
            .map(|e| e.action)
    }
}

/// Where a shortcut is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Global,
    Panels,
    Session,
    Timeline,
    Split,
    Terminal,
    Lists,
}

impl Group {
    pub const ALL: [Group; 7] = [
        Group::Global,
        Group::Panels,
        Group::Session,
        Group::Timeline,
        Group::Lists,
        Group::Split,
        Group::Terminal,
    ];

    pub fn title(self, language: Language) -> String {
        let en = match self {
            Group::Global => "Global",
            Group::Panels => "Panels",
            Group::Session => "Session",
            Group::Timeline => "Timeline",
            Group::Split => "Split view",
            Group::Terminal => "Terminal",
            Group::Lists => "Inbox and Tasks",
        };
        crate::ui::copy::t(language, en)
    }
}

/// Who handles the chord: the window's `on_key` through [`dispatch`], the
/// session composer (its submit keys), or the terminal panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Window,
    Composer,
    Terminal,
}

/// One row of the keymap.
pub struct Entry {
    pub action: Action,
    pub group: Group,
    pub owner: Owner,
    /// Rune keybind spec, alternatives comma-separated.
    spec: &'static str,
    /// The chords on macOS, where they differ.
    mac: Option<&'static str>,
    pub en: &'static str,
}

const fn e(action: Action, group: Group, spec: &'static str, en: &'static str) -> Entry {
    Entry {
        action,
        group,
        owner: Owner::Window,
        spec,
        mac: None,
        en,
    }
}

const fn owned(mut entry: Entry, owner: Owner) -> Entry {
    entry.owner = owner;
    entry
}

const fn on_mac(mut entry: Entry, mac: &'static str) -> Entry {
    entry.mac = Some(mac);
    entry
}

use Action as A;
use Group as G;

/// The keymap. One row per command; its first chord is the one hints show.
pub static ENTRIES: &[Entry] = &[
    e(A::Shortcuts, G::Global, "mod+/, ?", "Keyboard shortcuts"),
    e(
        A::Palette,
        G::Global,
        "mod+k, mod+shift+p",
        "Command palette",
    ),
    e(A::QuickOpen, G::Global, "mod+p", "Go to session…"),
    e(A::NewSession, G::Global, "mod+n", "New session"),
    e(A::Home, G::Global, "mod+shift+h", "Home"),
    e(A::Inbox, G::Global, "mod+shift+i", "Inbox"),
    e(A::Tasks, G::Global, "mod+shift+t", "Tasks"),
    e(A::Settings, G::Global, "mod+comma", "Settings"),
    e(
        A::Back,
        G::Global,
        "escape",
        "Close the innermost popup or the search, then back to the session",
    ),
    e(
        A::ToggleSidebar,
        G::Panels,
        "mod+b",
        "Show or hide the sidebar",
    ),
    e(
        A::ToggleTerminal,
        G::Panels,
        "ctrl+`",
        "Show or hide the terminal",
    ),
    e(
        A::ToggleFocus,
        G::Panels,
        "mod+;",
        "Switch focus between session and terminal",
    ),
    e(A::NewShell, G::Panels, "ctrl+shift+`", "New shell"),
    e(
        A::ToggleInspector,
        G::Panels,
        "mod+alt+b",
        "Show or hide the inspector or detail pane",
    ),
    e(
        A::ToggleGit,
        G::Panels,
        "mod+alt+g",
        "Show or hide Git inspector",
    ),
    e(A::FocusSearch, G::Panels, "mod+f", "Search sessions"),
    e(
        A::SearchMessages,
        G::Panels,
        "mod+shift+f",
        "Search in messages",
    ),
    e(
        A::ToggleRecentSession,
        G::Panels,
        "ctrl+tab",
        "Toggle recent session",
    ),
    e(
        A::PrevSession,
        G::Panels,
        "alt+up",
        "Previous session in the sidebar",
    ),
    e(
        A::NextSession,
        G::Panels,
        "alt+down",
        "Next session in the sidebar",
    ),
    e(
        A::FocusComposer,
        G::Session,
        "mod+l",
        "Focus the message field",
    ),
    owned(
        e(
            A::ExpandComposer,
            G::Session,
            "mod+shift+e",
            "Expand or collapse the composer",
        ),
        Owner::Composer,
    ),
    owned(
        e(
            A::PromptHistory,
            G::Session,
            "alt+h, ctrl+r",
            "Prompt history",
        ),
        Owner::Composer,
    ),
    owned(
        e(
            A::Send,
            G::Session,
            "mod+enter",
            "Send (steer while the agent works)",
        ),
        Owner::Composer,
    ),
    owned(
        e(
            A::Queue,
            G::Session,
            "alt+enter",
            "Queue after the running turn",
        ),
        Owner::Composer,
    ),
    e(A::Stop, G::Session, "mod+period", "Stop the agent"),
    e(
        A::ModelPicker,
        G::Session,
        "mod+shift+m",
        "Choose the model",
    ),
    e(
        A::Dictation,
        G::Session,
        "mod+shift+space",
        "Start or finish dictation",
    ),
    e(
        A::JumpLatest,
        G::Session,
        "end, mod+end",
        "Jump to the latest message",
    ),
    e(A::NextRow, G::Timeline, "j", "Next row"),
    e(A::PrevRow, G::Timeline, "k", "Previous row"),
    e(A::ToggleRow, G::Timeline, "space", "Open or fold the row"),
    e(A::OpenRow, G::Timeline, "enter", "Show the row's details"),
    e(A::NextMessage, G::Timeline, "]", "Next message"),
    e(A::PrevMessage, G::Timeline, "[", "Previous message"),
    e(A::Allow, G::Lists, "a", "Allow"),
    e(A::AllowAlways, G::Lists, "shift+a", "Always allow"),
    e(A::Deny, G::Lists, "d", "Deny"),
    e(A::MarkDone, G::Lists, "m", "Mark the paused task done"),
    e(
        A::CancelTask,
        G::Lists,
        "x",
        "Cancel the task (press twice)",
    ),
    e(
        A::AddSplit,
        G::Split,
        "mod+\\",
        "Add a session to split view",
    ),
    e(
        A::RemoveSplit,
        G::Split,
        "mod+w",
        "Remove the pane from split view",
    ),
    e(
        A::FocusPane,
        G::Split,
        "mod+1, mod+2, mod+3, mod+4",
        "Focus pane 1–4",
    ),
    e(
        A::FocusPaneToward,
        G::Split,
        "mod+alt+left, mod+alt+right, mod+alt+up, mod+alt+down",
        "Focus the pane that way",
    ),
    e(
        A::SwapPaneToward,
        G::Split,
        "mod+alt+shift+left, mod+alt+shift+right, mod+alt+shift+up, mod+alt+shift+down",
        "Swap the pane with the one that way",
    ),
    owned(
        e(
            A::TermPrefix,
            G::Terminal,
            "ctrl+space",
            "Herdr prefix (c new, k close, n/p tab, h/v split, [ find, ; focus)",
        ),
        Owner::Terminal,
    ),
    owned(
        on_mac(
            e(
                A::TermNewTab,
                G::Terminal,
                "ctrl+shift+t",
                "New terminal tab",
            ),
            "mod+t, ctrl+shift+t",
        ),
        Owner::Terminal,
    ),
    owned(
        e(
            A::TermCloseTab,
            G::Terminal,
            "ctrl+shift+w, alt+escape",
            "Close terminal tab or pane",
        ),
        Owner::Terminal,
    ),
    owned(
        e(
            A::TermSwitchTab,
            G::Terminal,
            "alt+1, alt+2, alt+3, alt+4, alt+5, alt+6, alt+7, alt+8, alt+9",
            "Switch to terminal tab 1–9",
        ),
        Owner::Terminal,
    ),
    owned(
        e(
            A::TermCycleTab,
            G::Terminal,
            "alt+left, alt+right, ctrl+tab, ctrl+shift+tab",
            "Previous or next terminal tab",
        ),
        Owner::Terminal,
    ),
    owned(
        on_mac(
            e(
                A::TermSplit,
                G::Terminal,
                "ctrl+shift+d, alt+enter",
                "Split terminal pane",
            ),
            "mod+d, alt+enter",
        ),
        Owner::Terminal,
    ),
    owned(
        on_mac(
            e(
                A::TermSearch,
                G::Terminal,
                "ctrl+shift+f",
                "Find in terminal scrollback",
            ),
            "mod+f, ctrl+shift+f",
        ),
        Owner::Terminal,
    ),
    owned(
        on_mac(e(A::TermCopy, G::Terminal, "ctrl+shift+c", "Copy"), "mod+c"),
        Owner::Terminal,
    ),
    owned(
        on_mac(
            e(
                A::TermPaste,
                G::Terminal,
                "ctrl+shift+v, shift+insert",
                "Paste",
            ),
            "mod+v, shift+insert",
        ),
        Owner::Terminal,
    ),
];

impl Entry {
    /// The built-in default keybind spec string.
    pub fn default_spec(&self) -> &'static str {
        self.spec
    }

    /// The built-in default `Keybind`.
    pub fn default_keybind(&self) -> &'static Keybind {
        static BINDS: OnceLock<Vec<Keybind>> = OnceLock::new();
        let binds = BINDS.get_or_init(|| {
            ENTRIES
                .iter()
                .map(|e| {
                    let bind = Keybind::new(e.spec);
                    match e.mac {
                        Some(mac) => bind.on(KeyPlatform::Mac, mac),
                        None => bind,
                    }
                })
                .collect()
        });
        let at = ENTRIES
            .iter()
            .position(|e| std::ptr::eq(e, self))
            .expect("an entry of ENTRIES");
        &binds[at]
    }

    /// The effective spec string for this entry, respecting user overrides in `cfg`.
    #[allow(dead_code)]
    pub fn effective_spec<'a>(&'a self, cfg: Option<&'a KeymapConfig>) -> &'a str {
        if let Some(cfg) = cfg {
            if let Some(custom) = cfg.bindings.get(self.action.id()) {
                if try_parse_keybind(custom).is_some() {
                    return custom.as_str();
                }
            }
            if self.action == Action::TermPrefix && try_parse_keybind(&cfg.leader).is_some() {
                return cfg.leader.as_str();
            }
        }
        self.spec
    }

    /// Whether this entry has a valid custom override in `cfg`.
    pub fn is_overridden(&self, cfg: &KeymapConfig) -> bool {
        if let Some(custom) = cfg.bindings.get(self.action.id()) {
            if try_parse_keybind(custom).is_some() && custom.trim() != self.spec {
                return true;
            }
        }
        if self.action == Action::TermPrefix
            && try_parse_keybind(&cfg.leader).is_some()
            && cfg.leader.trim() != self.spec
        {
            return true;
        }
        false
    }

    /// Resolve the `Keybind` for this entry, respecting user overrides in `cfg`
    /// (or the active keymap snapshot when `cfg` is `None`).
    pub fn keybind_for(&self, cfg: Option<&KeymapConfig>) -> Keybind {
        if let Some(cfg) = cfg {
            if let Some(custom) = cfg.bindings.get(self.action.id()) {
                if let Some(bind) = try_parse_keybind(custom) {
                    return bind;
                }
            }
            if self.action == Action::TermPrefix {
                if let Some(bind) = try_parse_keybind(&cfg.leader) {
                    return bind;
                }
            }
            self.default_keybind().clone()
        } else {
            ACTIVE_KEYMAP.with(|cell| self.keybind_for(Some(&cell.borrow())))
        }
    }

    /// The active `Keybind` for this entry.
    pub fn keybind(&self) -> Keybind {
        self.keybind_for(None)
    }

    /// What it does, in the owner's language.
    pub fn what(&self, language: Language) -> String {
        crate::ui::copy::t(language, self.en)
    }

    /// Each chord's keycaps on `platform`.
    pub fn keycaps_on(&self, platform: KeyPlatform) -> Vec<Vec<String>> {
        keycaps_for(self, platform, None)
    }
}

pub fn entry(action: Action) -> &'static Entry {
    ENTRIES
        .iter()
        .find(|e| e.action == action)
        .expect("every action has a row")
}

/// Each chord's keycaps for `entry` on `platform`, respecting `cfg`.
pub fn keycaps_for(
    entry: &Entry,
    platform: KeyPlatform,
    cfg: Option<&KeymapConfig>,
) -> Vec<Vec<String>> {
    entry
        .keybind_for(cfg)
        .chords_on(platform)
        .iter()
        .map(|c| c.keycaps_on(platform))
        .collect()
}

/// The chord hints show for `action` ("Ctrl+B", "⌘B") with explicit `cfg`.
pub fn label_with(action: Action, cfg: Option<&KeymapConfig>) -> String {
    entry(action)
        .keybind_for(cfg)
        .chords()
        .first()
        .map(|c| c.label())
        .unwrap_or_default()
}

/// The chord hints show for `action` ("Ctrl+B", "⌘B").
pub fn label(action: Action) -> String {
    label_with(action, None)
}

/// Every chord of `action`, in the keymap's order, with explicit `cfg`.
pub fn labels_with(action: Action, cfg: Option<&KeymapConfig>) -> Vec<String> {
    entry(action)
        .keybind_for(cfg)
        .chords()
        .iter()
        .map(|c| c.label())
        .collect()
}

/// Every chord of `action`, in the keymap's order (the pane arrows: left,
/// right, up, down).
pub fn labels(action: Action) -> Vec<String> {
    labels_with(action, None)
}

/// Resolve the active Leader `Keybind` from `cfg` (or the active keymap),
/// falling back to `ctrl+space` if malformed.
pub fn leader_keybind(cfg: Option<&KeymapConfig>) -> Keybind {
    let resolve = |c: &KeymapConfig| {
        try_parse_keybind(&c.leader)
            .or_else(|| {
                c.bindings
                    .get(Action::TermPrefix.id())
                    .and_then(|s| try_parse_keybind(s))
            })
            .unwrap_or_else(|| Keybind::new(KeymapConfig::DEFAULT_LEADER))
    };
    match cfg {
        Some(c) => resolve(c),
        None => ACTIVE_KEYMAP.with(|cell| resolve(&cell.borrow())),
    }
}

/// Keycaps for the active Leader key on `platform`.
pub fn leader_keycaps(platform: KeyPlatform, cfg: Option<&KeymapConfig>) -> Vec<String> {
    leader_keybind(cfg)
        .chords_on(platform)
        .first()
        .map(|c| c.keycaps_on(platform))
        .unwrap_or_else(|| vec!["Ctrl".into(), "Space".into()])
}

/// Display label for the active Leader key (e.g. `"Ctrl+Space"`).
pub fn leader_label(cfg: Option<&KeymapConfig>) -> String {
    leader_keybind(cfg)
        .chords()
        .first()
        .map(|c| c.label())
        .unwrap_or_else(|| "Ctrl+Space".to_string())
}

/// A button's tooltip: "Toggle sidebar (Ctrl+B)".
pub fn tip(text: &str, action: Action) -> String {
    format!("{text} ({})", label(action))
}

/// The sidebar toggle's chord as a `&'static str`, as the kit's button takes it.
pub fn sidebar_label() -> &'static str {
    static INTERNED: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let cur = label(Action::ToggleSidebar);
    let mut list = INTERNED.lock().unwrap();
    if let Some(&s) = list.iter().find(|&&s| s == cur) {
        return s;
    }
    let leaked: &'static str = Box::leak(cur.into_boxed_str());
    list.push(leaked);
    leaked
}

/// Two rows sharing a chord on `platform` with explicit `cfg`, among rows `pick` keeps.
pub fn clashes_for(
    platform: KeyPlatform,
    cfg: Option<&KeymapConfig>,
    pick: impl Fn(&Entry) -> bool,
) -> Vec<(Action, Action, String)> {
    let rows: Vec<&Entry> = ENTRIES.iter().filter(|e| pick(e)).collect();
    let mut out = Vec::new();
    for (i, a) in rows.iter().enumerate() {
        let bind_a = a.keybind_for(cfg);
        for b in &rows[i + 1..] {
            let bind_b = b.keybind_for(cfg);
            for ca in bind_a.chords_on(platform) {
                if bind_b
                    .chords_on(platform)
                    .iter()
                    .any(|cb| ca.same_keys(cb, platform))
                {
                    out.push((a.action, b.action, ca.label_on(platform)));
                }
            }
        }
    }
    out
}

/// Two rows sharing a chord on `platform`, among rows `pick` keeps.
pub fn clashes(
    platform: KeyPlatform,
    pick: impl Fn(&Entry) -> bool,
) -> Vec<(Action, Action, String)> {
    clashes_for(platform, None, pick)
}

/// Inside the terminal on Windows and Linux, a window chord with Ctrl
/// arrives with Shift added; one that already has Shift arrives as it is.
/// The window chords that land on the same keys there, the first one wins
/// (`(winner, shadowed, keys)`). macOS sends ⌘ chords unchanged.
pub fn terminal_shadows_for(
    platform: KeyPlatform,
    cfg: Option<&KeymapConfig>,
) -> Vec<(Action, Action, String)> {
    if platform == KeyPlatform::Mac {
        return Vec::new();
    }
    let mut seen: Vec<(rune::Chord, Action, bool)> = Vec::new();
    let mut out = Vec::new();
    // Ctrl+` is the terminal's own toggle there, unshifted.
    let lent = ENTRIES
        .iter()
        .filter(|e| e.owner == Owner::Window && e.action != Action::ToggleTerminal);
    for e in lent {
        let bind = e.keybind_for(cfg);
        for chord in bind.chords_on(platform) {
            if !chord.has(rune::ChordMod::Primary) && !chord.has(rune::ChordMod::Ctrl) {
                continue;
            }
            let exact = chord.has(rune::ChordMod::Shift);
            let there = chord.with(rune::ChordMod::Shift);
            match seen.iter().find(|(c, ..)| c.same_keys(&there, platform)) {
                Some((_, other, other_exact)) if *other != e.action => {
                    let (winner, shadowed) = if *other_exact || !exact {
                        (*other, e.action)
                    } else {
                        (e.action, *other)
                    };
                    out.push((winner, shadowed, there.label_on(platform)));
                }
                Some(_) => {}
                None => seen.push((there, e.action, exact)),
            }
        }
    }
    out
}

pub fn terminal_shadows(platform: KeyPlatform) -> Vec<(Action, Action, String)> {
    terminal_shadows_for(platform, None)
}

/// Convert a pressed `KeyEvent` into a normalized Rune keybind spec string
/// (e.g. `"ctrl+shift+k"`, `"alt+p"`, `"mod+/"`, `"ctrl+space"`).
pub fn event_to_spec(ev: &KeyEvent, physical: Option<rune::PhysicalKeyCode>) -> Option<String> {
    let mut parts: Vec<&'static str> = Vec::new();
    let mac = crate::platform::is_macos();
    if ev.mods.ctrl {
        parts.push("ctrl");
        if mac && ev.mods.logo {
            parts.push("mod");
        }
    } else if ev.mods.logo || ev.mods.cmd {
        parts.push("mod");
    }
    if ev.mods.alt {
        parts.push("alt");
    }
    if ev.mods.shift {
        parts.push("shift");
    }

    let key_part: String = if physical == Some(rune::PhysicalKeyCode::Space)
        || matches!(&ev.key, rune::Key::Command(' ' | '\0'))
        || matches!(&ev.key, rune::Key::Text(t) if t == " " || t == "\0")
    {
        "space".to_string()
    } else if physical == Some(rune::PhysicalKeyCode::Backquote)
        || matches!(&ev.key, rune::Key::Command('`' | '~'))
        || matches!(&ev.key, rune::Key::Text(t) if t == "`" || t == "~")
    {
        "`".to_string()
    } else {
        match &ev.key {
            rune::Key::Command(',') => "comma".to_string(),
            rune::Key::Command('.') => "period".to_string(),
            rune::Key::Command(c) if c.is_ascii_alphabetic() => c.to_ascii_lowercase().to_string(),
            rune::Key::Command(c) if !c.is_control() => c.to_string(),
            rune::Key::Text(t) => {
                let mut chars = t.chars();
                let (Some(c), None) = (chars.next(), chars.next()) else {
                    return None;
                };
                if c == ',' {
                    "comma".to_string()
                } else if c == '.' {
                    "period".to_string()
                } else if c.is_ascii_alphabetic() {
                    c.to_ascii_lowercase().to_string()
                } else if !c.is_control() {
                    c.to_string()
                } else {
                    return None;
                }
            }
            rune::Key::Enter => "enter".to_string(),
            rune::Key::Tab => "tab".to_string(),
            rune::Key::Backspace => "backspace".to_string(),
            rune::Key::Delete => "delete".to_string(),
            rune::Key::Insert => "insert".to_string(),
            rune::Key::Home => "home".to_string(),
            rune::Key::End => "end".to_string(),
            rune::Key::PageUp => "pageup".to_string(),
            rune::Key::PageDown => "pagedown".to_string(),
            rune::Key::Left => "left".to_string(),
            rune::Key::Right => "right".to_string(),
            rune::Key::Up => "up".to_string(),
            rune::Key::Down => "down".to_string(),
            rune::Key::Escape => "escape".to_string(),
            rune::Key::Function(n) => format!("f{n}"),
            _ => return None,
        }
    };

    let spec = if parts.is_empty() {
        key_part
    } else {
        format!("{}+{key_part}", parts.join("+"))
    };
    try_parse_keybind(&spec)?;
    Some(spec)
}

/// If `s.recording_key` is active, handle `ev` (Esc cancels, any valid key records).
pub fn handle_recording_key(s: &mut UiState, ev: &KeyEvent, tx: &Tx) -> bool {
    let Some(target) = s.recording_key else {
        return false;
    };
    if ev.key == rune::Key::Escape
        && !(ev.mods.ctrl || ev.mods.alt || ev.mods.shift || ev.mods.logo)
    {
        s.recording_key = None;
        return true;
    }
    if let Some(spec) = event_to_spec(ev, tx.physical_key()) {
        s.recording_key = None;
        match target {
            RecordingTarget::Leader => {
                s.edit(crate::config::ConfigEdit::SetLeader(spec));
            }
            RecordingTarget::Action(action) => {
                if action == Action::TermPrefix {
                    s.edit(crate::config::ConfigEdit::SetLeader(spec.clone()));
                }
                let default_spec = entry(action).default_spec();
                let override_spec = if spec == default_spec {
                    None
                } else {
                    Some(spec)
                };
                s.edit(crate::config::ConfigEdit::SetKeybind {
                    action: action.id().to_string(),
                    spec: override_spec,
                });
            }
        }
    }
    true
}

/// Whether `ev` matches the configured global Leader key.
pub fn is_leader_event(s: &UiState, ev: &KeyEvent, tx: &Tx) -> bool {
    let leader_str = s.cfg.keymap.leader.trim();
    if (leader_str.is_empty() || leader_str.eq_ignore_ascii_case("ctrl+space"))
        && crate::term::input::is_ctrl_space(ev, tx.physical_key())
    {
        return true;
    }
    let bind = leader_keybind(Some(&s.cfg.keymap));
    keybind_matches(&bind, ev, tx)
}

/// Execute a follow-up chord when `leader_armed` is active.
pub fn handle_leader_key(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let was_term_focused = s.is_terminal_focused();
    s.disarm_leader();

    // Double-tap Leader key: disarm and forward literal key to terminal if focused.
    if is_leader_event(s, ev, tx) {
        if was_term_focused {
            let leader_str = s.cfg.keymap.leader.trim();
            if crate::term::input::is_ctrl_space(ev, tx.physical_key())
                || leader_str.is_empty()
                || leader_str.eq_ignore_ascii_case("ctrl+space")
            {
                s.term.send(crate::term::session::Msg::Text("\x00".into()));
            } else if let Some(key) =
                crate::term::input::key_input(ev, tx.physical_key(), tx.key_base(), tx.key_repeat())
            {
                s.term.send(crate::term::session::Msg::Key(key));
            }
        }
        return true;
    }

    if ev.key == rune::Key::Escape {
        return true;
    }

    if matches!(
        ev.key,
        rune::Key::Left | rune::Key::Right | rune::Key::Up | rune::Key::Down
    ) {
        if s.in_split() {
            if let Some(j) =
                super::dwindle::neighbour(&s.tile_layout(), s.tile_grid, s.focus, &ev.key)
            {
                s.focus_tile(j);
                if s.tiles.get(s.focus).is_some_and(|t| t.is_terminal()) {
                    s.focus_terminal_tx(tx);
                }
            }
        } else {
            match ev.key {
                rune::Key::Left => s.term_prev_tab(),
                rune::Key::Right => s.term_next_tab(),
                rune::Key::Up => {
                    let _ = step_session(s, false);
                }
                rune::Key::Down => {
                    let _ = step_session(s, true);
                }
                _ => {}
            }
        }
        return true;
    }

    let raw_ch = match &ev.key {
        rune::Key::Text(t) => t.chars().next(),
        rune::Key::Command(c) => Some(*c),
        _ => None,
    };
    let ch = match (raw_ch, ev.mods.shift) {
        (Some('/'), true) => Some('?'),
        (Some('\\'), true) => Some('|'),
        (Some(';'), true) => Some(':'),
        (Some(c), true) if c.is_ascii_alphabetic() => Some(c.to_ascii_uppercase()),
        (c, _) => c,
    };

    match ch {
        Some(' ' | ';') => s.toggle_focus_tx(tx),
        Some('k' | ':') => s.toggle_palette(false),
        Some('p' | 'o') => s.toggle_palette(true),
        Some('n') => {
            let _ = run(s, Action::NewSession, ev, tx);
        }
        Some('b') => s.sidebar.toggle(),
        Some('g') => {
            if !run(s, Action::ToggleGit, ev, tx) {
                s.open_git_inspector();
            }
        }
        Some('i') => {
            if !run(s, Action::ToggleInspector, ev, tx) {
                s.toggle_inspector();
            }
        }
        Some('t') => s.toggle_terminal_tx(tx),
        Some('c' | 'C') => {
            s.term_new_tab();
            s.focus_terminal_tx(tx);
        }
        Some('x' | 'X') => {
            if s.in_split() && s.tiles.get(s.focus).is_some_and(|t| t.is_terminal()) {
                s.close_tile(s.focus);
            } else if s.term.open || s.has_terminal_tile() {
                s.term_close_tab(s.term.active_tab);
            } else if s.in_split() {
                s.close_tile(s.focus);
            }
        }
        Some('[' | ',') => {
            if ch == Some('[') && (was_term_focused || s.term.open) {
                s.open_terminal_search();
            } else {
                let _ = step_session(s, false);
            }
        }
        Some(']') => {
            let _ = step_session(s, true);
        }
        Some('h' | 's' | '-') => {
            s.split_terminal_with_axis(false);
            s.focus_terminal_tx(tx);
        }
        Some('v' | 'V' | '\\' | '|') => {
            s.split_terminal_with_axis(true);
            s.focus_terminal_tx(tx);
        }
        Some('z' | 'Z') => s.toggle_terminal_zoom(),
        Some('w' | 'W') => {
            if s.in_split() {
                s.close_tile(s.focus);
            }
        }
        Some(d @ '1'..='4') => {
            let idx = (d as u8 - b'1') as usize;
            if s.in_split() {
                if idx < s.tiles.len() {
                    s.focus_tile(idx);
                    if s.tiles.get(idx).is_some_and(|t| t.is_terminal()) {
                        s.focus_terminal_tx(tx);
                    }
                }
            } else {
                s.term_switch_tab(idx);
            }
        }
        Some(d @ '5'..='9') => {
            let idx = (d as u8 - b'1') as usize;
            s.term_switch_tab(idx);
        }
        Some('/' | 'f' | 'F') => {
            if was_term_focused || s.term.open {
                s.open_terminal_search();
            } else {
                let _ = run(s, Action::FocusSearch, ev, tx);
            }
        }
        Some('H') => s.go(Mode::Home),
        Some('I') => s.go(Mode::Inbox),
        Some('T') => s.go(Mode::Tasks),
        Some('S') => s.go(Mode::Settings),
        Some('?') => s.toggle_keys_sheet(),
        Some('d' | 'D') => s.close_terminal(),
        _ => {}
    }
    true
}

/// Intercept shortcut recording and global Leader / Which-Key mode before
/// any field, terminal, or window key handler.
pub fn intercept_global_key(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    sync_active_keymap(&s.cfg.keymap);
    if handle_recording_key(s, ev, tx) {
        return true;
    }
    if s.is_leader_armed() {
        return handle_leader_key(s, ev, tx);
    }
    if is_leader_event(s, ev, tx) {
        s.arm_leader(tx);
        return true;
    }
    false
}

/// The window's keys (`on_key`): run the first row whose chord this is.
pub fn dispatch(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if intercept_global_key(s, ev, tx) {
        return true;
    }
    if crate::term::input::is_toggle(ev, tx.physical_key())
        || bind_matches_cfg(entry(A::ToggleTerminal), &s.cfg.keymap, ev, tx)
    {
        s.toggle_terminal_tx(tx);
        return true;
    }
    if s.show_diff
        && ev.key == rune::Key::Escape
        && !(ev.mods.shift || ev.mods.alt || ev.mods.ctrl || ev.mods.cmd)
        && s.palette.is_none()
        && s.keys_sheet.is_none()
        && !s.timeline_search.active
    {
        s.show_diff = false;
        return true;
    }
    if let Some(sel) = tx.text_selection() {
        let letter =
            |c: char| matches!(&ev.key, rune::Key::Command(k) if k.eq_ignore_ascii_case(&c));
        let is_copy =
            (ev.mods.ctrl || ev.mods.cmd) && !ev.mods.alt && !ev.mods.shift && letter('c');
        if is_copy {
            tx.copy(sel.text().to_string());
            return true;
        }
    }
    let term_active = s.is_terminal_focused();
    if term_active
        && !s.draft.focused
        && !s.start_draft.focused
        && !s.timeline_search.active
        && !s.term.search.open
        && !s.search.focused
        && s.palette.is_none()
        && s.keys_sheet.is_none()
    {
        return s.terminal_key(ev, tx);
    }
    dispatch_window(s, ev, tx)
}

pub(crate) fn keybind_matches(bind: &Keybind, ev: &KeyEvent, tx: &Tx) -> bool {
    if bind.matches_tx(ev, tx) {
        return true;
    }
    if matches!(&ev.key, rune::Key::Command('\0')) && ev.mods.ctrl {
        let norm = KeyEvent {
            key: rune::Key::Command(' '),
            mods: ev.mods,
        };
        if bind.matches_tx(&norm, tx) {
            return true;
        }
    }
    if let rune::Key::Text(t) = &ev.key {
        let mut chars = t.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let mapped = if c == '\0' {
                ' '
            } else {
                c.to_ascii_lowercase()
            };
            if ev.mods.ctrl
                || ev.mods.alt
                || ev.mods.cmd
                || ev.mods.logo
                || !c.is_ascii_alphabetic()
            {
                let norm = KeyEvent {
                    key: rune::Key::Command(mapped),
                    mods: ev.mods,
                };
                if bind.matches_tx(&norm, tx) {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn bind_matches_cfg(entry: &Entry, cfg: &KeymapConfig, ev: &KeyEvent, tx: &Tx) -> bool {
    keybind_matches(&entry.keybind_for(Some(cfg)), ev, tx)
}

/// Check registered plugin commands for a matching `shortcut` chord and execute it.
pub fn dispatch_plugin_shortcut(s: &mut UiState, ev: &KeyEvent, tx: &Tx) -> bool {
    let plugins = s.plugins.all();
    for plugin in plugins {
        for cmd in plugin.commands() {
            let Some(sc) = cmd.shortcut.map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(bind) = try_parse_keybind(sc) else {
                continue;
            };
            if keybind_matches(&bind, ev, tx) {
                s.keys_sheet = None;
                s.palette = None;
                return plugin.on_command(s, cmd.id);
            }
        }
    }
    false
}

/// Run a window shortcut (used by [`dispatch`] and by the terminal's
/// `Route::App` handoff so it does not loop back into `terminal_key`).
pub fn dispatch_window(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    sync_active_keymap(&s.cfg.keymap);
    // The arrows step the Inbox's and Tasks' lists as j and k do.
    if matches!(s.mode, Mode::Inbox | Mode::Tasks)
        && matches!(ev.key, rune::Key::Up | rune::Key::Down)
        && !(ev.mods.shift || ev.mods.alt || ev.mods.ctrl || ev.mods.cmd)
    {
        let action = if ev.key == rune::Key::Down {
            A::NextRow
        } else {
            A::PrevRow
        };
        return run(s, action, ev, tx);
    }
    if let Some(found) = ENTRIES
        .iter()
        .filter(|e| {
            e.owner == Owner::Window || matches!(e.action, A::ExpandComposer | A::PromptHistory)
        })
        .find(|e| bind_matches_cfg(e, &s.cfg.keymap, ev, tx))
    {
        return run(s, found.action, ev, tx);
    }
    if dispatch_plugin_shortcut(s, ev, tx) {
        return true;
    }
    false
}

/// Keys a composer hears before its editor, which would take them as
/// caret moves: Alt+↑/↓, the split's pane arrows, composer shortcuts,
/// focus toggles, and Ctrl+End once the caret is already at the end.
pub fn field_key(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx, at_end: bool) -> bool {
    if intercept_global_key(s, ev, tx) {
        return true;
    }
    let term_active = s.is_terminal_focused();
    if term_active && !s.draft.focused && !s.start_draft.focused {
        s.focus_terminal_tx(tx);
        return s.terminal_key(ev, tx);
    }
    for action in [
        A::PrevSession,
        A::NextSession,
        A::ToggleFocus,
        A::FocusPane,
        A::FocusPaneToward,
        A::SwapPaneToward,
        A::ExpandComposer,
        A::PromptHistory,
    ] {
        if bind_matches_cfg(entry(action), &s.cfg.keymap, ev, tx) {
            return run(s, action, ev, tx);
        }
    }
    if !cfg!(target_os = "macos")
        && ev.mods.ctrl
        && !ev.mods.shift
        && !ev.mods.alt
        && !ev.mods.logo
        && matches!(&ev.key, rune::Key::Command(c) if c.eq_ignore_ascii_case(&'e'))
    {
        let edit = if s.mode == Mode::Home {
            &mut s.start_draft
        } else {
            &mut s.draft
        };
        edit.key(
            &rune::KeyEvent {
                key: rune::Key::End,
                mods: rune::Mods::default(),
            },
            None,
        );
        return true;
    }
    let ctrl_end = ev.key == rune::Key::End && ev.mods.primary() && !ev.mods.shift;
    at_end && ctrl_end && run(s, A::JumpLatest, ev, tx)
}

/// Do what `action` says. `false`: it doesn't apply here, so the key goes on.
pub fn run(s: &mut UiState, action: Action, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // A command run from the keyboard leaves the sheet and the palette.
    if !matches!(action, A::Shortcuts | A::Palette | A::QuickOpen | A::Back) {
        s.keys_sheet = None;
        s.palette = None;
    }
    let session = s.mode == Mode::Session;
    match action {
        A::Shortcuts => s.toggle_keys_sheet(),
        A::Palette => s.toggle_palette(false),
        A::QuickOpen => s.toggle_palette(true),
        A::NewSession => {
            s.go(Mode::Home);
            s.start_draft.focused = true;
        }
        A::Home => s.go(Mode::Home),
        A::Inbox => s.go(Mode::Inbox),
        A::Tasks => s.go(Mode::Tasks),
        A::Settings => s.go(Mode::Settings),
        A::Back => return escape(s),
        A::ToggleSidebar => s.sidebar.toggle(),
        A::ToggleTerminal => s.toggle_terminal_tx(tx),
        A::ToggleFocus => s.toggle_focus_tx(tx),
        A::NewShell => {
            s.open_terminal();
            s.restart_terminal();
        }
        A::ToggleInspector => {
            if s.inspect.is_some() {
                s.close_inspect();
            } else if session && !s.in_split() {
                s.toggle_inspector();
            } else {
                return false;
            }
        }
        A::ToggleGit => {
            if session && !s.in_split() {
                s.toggle_git_inspector();
            } else if s.selected.is_some() {
                s.open_git_inspector();
            } else {
                return false;
            }
        }
        A::FocusSearch => {
            if s.mode == Mode::Session && s.selected.is_some() {
                s.toggle_timeline_search();
            } else {
                s.focus_search(false);
            }
        }
        A::SearchMessages => s.focus_search(true),
        A::ToggleRecentSession => s.toggle_recent_session(),
        A::PrevSession | A::NextSession => {
            return step_session(s, action == A::NextSession);
        }
        A::AddSplit => s.split(),
        A::RemoveSplit if s.in_split() => s.close_tile(s.focus),
        A::FocusPane => {
            let n = ['1', '2', '3', '4']
                .iter()
                .position(|&c| {
                    ev.is_command(c)
                        || matches!(&ev.key, rune::Key::Text(t) if t.len() == 1 && t.starts_with(c))
                })
                .unwrap_or(usize::MAX);
            if !(s.in_split() && n < s.tiles.len()) {
                return false;
            }
            s.focus_tile(n);
            if s.tiles.get(n).is_some_and(|t| t.is_terminal()) {
                s.focus_terminal_tx(tx);
            }
        }
        A::FocusPaneToward | A::SwapPaneToward => {
            let was_term = s.term.sink.focused;
            if was_term {
                s.term.sink.focused = false;
            }
            let handled = super::tiles::arrow_key(s, ev);
            if s.in_split() && s.tiles.get(s.focus).is_some_and(|t| t.is_terminal()) {
                s.focus_terminal_tx(tx);
            } else if was_term && !handled {
                s.term.sink.focused = true;
            }
            return handled;
        }
        A::FocusComposer => match s.mode {
            Mode::Session => {
                s.draft.focused = true;
                s.term.sink.focused = false;
            }
            Mode::Home => {
                s.start_draft.focused = true;
                s.term.sink.focused = false;
            }
            _ => return false,
        },
        A::ExpandComposer if session => {
            if s.composer_expanded || s.composer_rows.is_some() {
                s.composer_expanded = false;
                s.composer_rows = None;
            } else {
                s.composer_expanded = true;
                s.composer_rows = Some(14);
            }
        }
        A::PromptHistory if session => {
            s.composer_history_open = !s.composer_history_open;
        }
        A::Stop if session && s.busy() => {
            let Some(asid) = s.selected.clone() else {
                return false;
            };
            s.send(crate::model::commands::Command::Interrupt(
                muqun_gateway::agents::AgentSessionId(asid),
            ));
        }
        A::ModelPicker => {
            let (menu, models) = match s.mode {
                Mode::Session => (OpenMenu::Model, s.catalog.as_deref()),
                Mode::Home => (OpenMenu::StartModel, s.start_catalog.as_deref()),
                _ => return false,
            };
            if models.is_none_or(|c| super::composer::catalog_models(c).is_empty()) {
                return false;
            }
            s.menu = (s.menu != Some(menu.clone())).then_some(menu);
        }
        A::Dictation if session => s.toggle_voice(),
        A::JumpLatest if session => {
            let Some(asid) = s.selected.clone() else {
                return false;
            };
            s.jump_to_latest(&asid, tx);
        }
        A::NextRow | A::PrevRow | A::OpenRow | A::Allow | A::AllowAlways | A::Deny
            if s.mode == Mode::Inbox =>
        {
            return super::inbox::key(s, action)
        }
        A::NextRow | A::PrevRow | A::OpenRow | A::MarkDone | A::CancelTask
            if s.mode == Mode::Tasks =>
        {
            return super::tasks::key(s, action)
        }
        A::NextRow | A::PrevRow | A::ToggleRow | A::OpenRow | A::NextMessage | A::PrevMessage => {
            return s.timeline_key(action, tx)
        }
        _ => return false,
    }
    true
}

/// Alt+↑ / Alt+↓: the session above or below the open one in the sidebar,
/// the first or last when none of them is open.
fn step_session(s: &mut UiState, down: bool) -> bool {
    let list = super::sessions::listed(s);
    if list.is_empty() {
        return false;
    }
    let here = s
        .selected
        .as_deref()
        .filter(|_| s.mode == Mode::Session)
        .and_then(|asid| list.iter().position(|a| a == asid));
    let at = match (here, down) {
        (Some(i), true) => (i + 1).min(list.len() - 1),
        (Some(i), false) => i.saturating_sub(1),
        (None, true) => 0,
        (None, false) => list.len() - 1,
    };
    let asid = list[at].clone();
    s.open_session(&asid);
    true
}

/// What Esc closes next, innermost first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscStep {
    /// The shortcuts sheet or the command palette.
    Sheet,
    /// A dropdown, a row's context menu or the context sheet.
    Popup,
    /// Dictation, the file viewer, a zoomed picture, a subagent's sheet.
    Dialog,
    /// Working tree changes diff view: return to chat.
    Diff,
    /// The timeline row's detail pane.
    Pane,
    /// In-session timeline search.
    TimelineSearch,
    /// In-terminal scrollback search.
    TermSearch,
    /// The sidebar's search: cleared, back to the session list.
    Search,
    /// An error alert banner.
    Alert,
    /// A page other than the session: back to the last session (or Home).
    Page,
}

/// The step Esc would take now; `None` leaves it alone.
pub fn esc_step(s: &UiState) -> Option<EscStep> {
    if s.pairing_target.is_some() {
        return Some(EscStep::Dialog);
    }
    if s.keys_sheet.is_some() || s.palette.is_some() {
        return Some(EscStep::Sheet);
    }
    if s.menu.is_some() || s.row_menu.is_some() || s.context_open.is_some() {
        return Some(EscStep::Popup);
    }
    if s.voice.is_some() || s.viewer.is_some() || s.zoom.is_some() || s.detail.is_some() {
        return Some(EscStep::Dialog);
    }
    if s.show_diff {
        return Some(EscStep::Diff);
    }
    if s.inspect.is_some() || (s.right_open && s.active_plugin_drawer.is_some()) {
        return Some(EscStep::Pane);
    }
    if s.term.search.open {
        return Some(EscStep::TermSearch);
    }
    if s.timeline_search.active {
        return Some(EscStep::TimelineSearch);
    }
    if s.searching() {
        return Some(EscStep::Search);
    }
    if s.error.is_some() {
        return Some(EscStep::Alert);
    }
    if s.active_plugin_page.is_some() {
        return Some(EscStep::Page);
    }
    let session = last_session(s).is_some();
    match s.mode {
        Mode::Session => None,
        Mode::Home if !session => None,
        _ => Some(EscStep::Page),
    }
}

/// The session Esc goes back to: the selected one, if it still exists.
fn last_session(s: &UiState) -> Option<String> {
    s.selected
        .clone()
        .filter(|asid| s.session_by_id(asid).is_some())
}

/// Esc with no field to blur: close the innermost thing open, else leave
/// the page for the last session (Home when there is none).
pub fn escape(s: &mut UiState) -> bool {
    let Some(step) = esc_step(s) else {
        return false;
    };
    match step {
        EscStep::Sheet => {
            if s.keys_sheet.take().is_none() {
                s.palette = None;
            }
        }
        EscStep::Popup => {
            if s.menu.take().is_none() && s.row_menu.take().is_none() {
                s.context_open = None;
            }
        }
        EscStep::Dialog => {
            if s.pairing_target.is_some() {
                s.close_pairing();
            } else if s.voice.is_some() {
                s.cancel_voice();
            } else if s.viewer.is_some() {
                s.close_viewer();
            } else if s.zoom.take().is_none() {
                s.close_detail();
            }
        }
        EscStep::Diff => s.show_diff = false,
        EscStep::Pane => {
            if s.inspect.is_some() {
                s.close_inspect();
            } else {
                s.close_plugin_drawer();
            }
        }
        EscStep::TimelineSearch => s.close_timeline_search(),
        EscStep::TermSearch => s.close_terminal_search_no_tx(),
        EscStep::Search => s.close_search(),
        EscStep::Alert => s.error = None,
        EscStep::Page => {
            if s.active_plugin_page.take().is_some()
                && s.mode == Mode::Session
                && s.selected.is_some()
            {
                return true;
            }
            match last_session(s) {
                Some(_) if s.in_split() => s.mode = Mode::Session,
                Some(asid) => s.open_session(&asid),
                None => s.go(Mode::Home),
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;

    const PLATFORMS: [KeyPlatform; 3] =
        [KeyPlatform::Mac, KeyPlatform::Windows, KeyPlatform::Linux];

    #[test]
    fn no_two_commands_share_a_chord_in_any_context() {
        for platform in PLATFORMS {
            for owner in [Owner::Window, Owner::Composer, Owner::Terminal] {
                // The composer's keys are heard where the window's are too;
                // the terminal captures all keys and only forwards unbound
                // `Route::App` chords to the window.
                let clashes = clashes(platform, |e| {
                    if owner == Owner::Terminal {
                        e.owner == Owner::Terminal
                    } else {
                        e.owner == owner || e.owner == Owner::Window
                    }
                });
                assert!(clashes.is_empty(), "{platform:?} {owner:?}: {clashes:?}");
            }
        }
        // Inside the terminal Ctrl+Shift+P is the palette's own chord, so
        // Ctrl+P (quick open) can't borrow it there, and Ctrl+Shift+F is the
        // message search's, so Ctrl+F can't. The pane arrows arrive shifted
        // too, and `tiles::arrow_key` only moves focus from there. Nothing
        // else collides.
        for platform in [KeyPlatform::Windows, KeyPlatform::Linux] {
            let mut want = vec![
                (A::Palette, A::QuickOpen, "Ctrl+Shift+P".to_string()),
                (
                    A::SearchMessages,
                    A::FocusSearch,
                    "Ctrl+Shift+F".to_string(),
                ),
            ];
            for arrow in ["←", "→", "↑", "↓"] {
                want.push((
                    A::SwapPaneToward,
                    A::FocusPaneToward,
                    format!("Ctrl+Alt+Shift+{arrow}"),
                ));
            }
            assert_eq!(terminal_shadows(platform), want);
        }
        assert!(terminal_shadows(KeyPlatform::Mac).is_empty());
    }

    #[test]
    fn the_terminal_keeps_its_rows_and_lends_ctrl_shift_to_the_window() {
        use crate::term::input::{route, Route};
        let key = |k: rune::Key, ctrl: bool, shift: bool| KeyEvent {
            key: k,
            mods: rune::Mods {
                shift,
                cmd: if crate::platform::is_macos() {
                    false
                } else {
                    ctrl
                },
                ctrl,
                ..Default::default()
            },
        };
        let copy = key(rune::Key::Command('c'), true, true);
        assert!(
            entry(A::TermCopy).keybind().chords_on(KeyPlatform::Linux)[0]
                .matches_on(&copy, KeyPlatform::Linux)
        );
        assert_eq!(route(&copy, None, false), Route::Copy);
        let paste = key(rune::Key::Command('v'), true, true);
        assert_eq!(route(&paste, None, false), Route::Paste);
        // Ctrl+B is the shell's; Ctrl+Shift+B reaches the window.
        assert_eq!(
            route(&key(rune::Key::Command('b'), true, false), None, false),
            Route::Terminal
        );
        assert_eq!(
            route(&key(rune::Key::Command('b'), true, true), None, false),
            Route::App
        );
        assert_eq!(
            route(&key(rune::Key::Escape, false, false), None, false),
            Route::Terminal
        );
    }

    fn state() -> UiState {
        let (tx, _rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.splash = None;
        let (_, scene) = super::super::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "session")
            .expect("the session scene");
        scene(&mut s);
        s
    }

    #[test]
    fn esc_closes_popup_then_pane_then_page_then_returns_to_the_session() {
        let mut s = state();
        let asid = s.selected.clone().expect("a session");
        s.go(Mode::Inbox);
        s.inspect = Some(super::super::Inspect {
            asid: asid.clone(),
            id: "x".into(),
        });
        s.menu = Some(OpenMenu::SessionMore);
        s.palette = Some(super::super::palette::Palette::new(false));
        s.search = rune::TextEdit::new("greet");
        s.search_changed();
        let mut steps = Vec::new();
        while let Some(step) = esc_step(&s) {
            steps.push(step);
            assert!(escape(&mut s));
        }
        assert_eq!(
            steps,
            [
                EscStep::Sheet,
                EscStep::Popup,
                EscStep::Pane,
                EscStep::Search,
                EscStep::Page
            ]
        );
        assert!(s.search.text.is_empty() && !s.search.focused);
        assert_eq!(s.mode, Mode::Session);
        assert_eq!(s.selected.as_deref(), Some(asid.as_str()));
        // On the session itself Esc has nothing left to do.
        assert!(!escape(&mut s));
        // With no session to return to, a page goes Home.
        s.selected = None;
        s.go(Mode::Settings);
        assert!(escape(&mut s));
        assert_eq!(s.mode, Mode::Home);
        assert_eq!(esc_step(&s), None);

        // Alert dismiss via Esc
        s.error = Some(crate::model::inbox::UiError {
            message: "Network request failed".into(),
            detail: None,
            unreachable: false,
        });
        assert_eq!(esc_step(&s), Some(EscStep::Alert));
        assert!(escape(&mut s));
        assert!(s.error.is_none());
        assert_eq!(esc_step(&s), None);

        // Diff view dismiss via Esc
        s.show_diff = true;
        assert_eq!(esc_step(&s), Some(EscStep::Diff));
        assert!(escape(&mut s));
        assert!(!s.show_diff);
        assert_eq!(esc_step(&s), None);
    }

    #[test]
    fn focused_terminal_keeps_ctrl_b_p_w_for_shell_and_lends_ctrl_shift_and_split_arrows() {
        use rune::{Env, Key, Mods, Runtime};
        let mut s = state();
        s.open_terminal();
        assert!(s.term.open && s.term.sink.focused);
        let sidebar_was = s.sidebar.is_open();

        let mut rt = Runtime::new(s, super::super::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);

        let key = |k: Key, ctrl: bool, shift: bool, alt: bool| KeyEvent {
            key: k,
            mods: Mods {
                shift,
                cmd: ctrl,
                alt,
                ctrl,
                logo: false,
            },
        };

        // Direct dispatch and runtime key with Ctrl+B, Ctrl+P, Ctrl+W when terminal is focused:
        // Must go to the terminal shell, NOT stolen by ToggleSidebar / QuickOpen / RemoveSplit.
        for c in ['b', 'p', 'w'] {
            rt.key(key(Key::Command(c), true, false, false));
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.sidebar.is_open(), sidebar_was);
        assert!(rt.state.palette.is_none());
        assert_eq!(
            *rt.state.term.sent_log.borrow(),
            ["key B ctrl=true", "key P ctrl=true", "key W ctrl=true",]
        );

        // Ctrl+Shift+B toggles sidebar; Ctrl+Shift+K opens palette.
        if !crate::platform::is_macos() {
            rt.key(key(Key::Command('b'), true, true, false));
            rt.frame(1.0 / 60.0);
            assert_ne!(rt.state.sidebar.is_open(), sidebar_was);
        }

        // Split view navigation (`Ctrl+Alt+Arrow`) works from inside a terminal tile.
        rt.state.open_terminal_tile();
        assert!(rt.state.in_split());
        assert_eq!(rt.state.focus, 1);
        assert!(rt.state.term.sink.focused);
        rt.frame(1.0 / 60.0);

        rt.key(key(Key::Left, true, false, true));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.focus, 0);
        assert!(rt.state.draft.focused);
        assert!(!rt.state.term.sink.focused);

        // And from the session composer, `Ctrl+Alt+Right` moves focus back to the terminal tile!
        rt.key(key(Key::Right, true, false, true));
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.focus, 1);
        assert!(rt.state.term.sink.focused);

        // Ctrl+Shift+K from inside the terminal tile opens the command palette.
        if !crate::platform::is_macos() {
            rt.key(key(Key::Command('k'), true, true, false));
            rt.frame(1.0 / 60.0);
            assert!(rt.state.palette.is_some());
        }
    }

    #[test]
    fn action_ids_round_trip_and_malformed_specs_fallback_safely() {
        for e in ENTRIES {
            let id = e.action.id();
            assert!(!id.is_empty());
            assert_eq!(Action::from_id(id), Some(e.action));
        }
        assert_eq!(Action::from_id("nonexistent_action"), None);

        let mut cfg = KeymapConfig {
            leader: "not+a+valid+keybind+at+all!!!".into(),
            ..Default::default()
        };
        cfg.bindings
            .insert("palette".into(), "bogus+unknown_key".into());
        assert_eq!(
            entry(A::Palette).effective_spec(Some(&cfg)),
            "mod+k, mod+shift+p"
        );
        let bind = entry(A::Palette).keybind_for(Some(&cfg));
        assert!(!bind.chords().is_empty());
        let leader = leader_keybind(Some(&cfg));
        assert!(!leader.chords().is_empty());

        // `+,` is normalized to `+comma`
        assert!(try_parse_keybind("ctrl+,").is_some());
    }

    #[test]
    fn recording_custom_keybind_and_dispatching_custom_action_and_conflicts() {
        use crate::config::ConfigEdit;
        use rune::{Env, Key, Mods, Runtime};

        let mut s = state();
        s.mode = Mode::Settings;
        s.settings_tab = crate::ui::SettingsTab::Shortcuts;

        // Start recording for Palette
        s.recording_key = Some(RecordingTarget::Action(A::Palette));

        let mut rt = Runtime::new(s, super::super::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);

        // Pressing Esc cancels recording without modifying bindings
        rt.key(KeyEvent {
            key: Key::Escape,
            mods: Mods::default(),
        });
        rt.frame(1.0 / 60.0);
        assert!(rt.state.recording_key.is_none());
        assert!(rt.state.cfg.keymap.bindings.is_empty());

        // Arm recording for Palette again and press Alt+P
        rt.state.recording_key = Some(RecordingTarget::Action(A::Palette));
        let alt_p = KeyEvent {
            key: Key::Command('p'),
            mods: Mods {
                alt: true,
                ..Default::default()
            },
        };
        rt.key(alt_p.clone());
        rt.frame(1.0 / 60.0);

        assert!(rt.state.recording_key.is_none());
        assert_eq!(
            rt.state
                .cfg
                .keymap
                .bindings
                .get("palette")
                .map(String::as_str),
            Some("alt+p")
        );
        assert!(entry(A::Palette).is_overridden(&rt.state.cfg.keymap));

        // Now pressing Alt+P opens the Command Palette!
        assert!(rt.state.palette.is_none());
        rt.key(alt_p);
        rt.frame(1.0 / 60.0);
        assert!(rt.state.palette.is_some());
        rt.state.palette = None;

        // Setting QuickOpen to "alt+p" as well triggers a conflict in clashes()
        rt.state.edit(ConfigEdit::SetKeybind {
            action: A::QuickOpen.id().into(),
            spec: Some("alt+p".into()),
        });
        let found_clashes = clashes(KeyPlatform::current(), |e| e.owner == Owner::Window);
        assert!(
            found_clashes
                .iter()
                .any(|(a, b, _)| (*a == A::Palette && *b == A::QuickOpen)
                    || (*a == A::QuickOpen && *b == A::Palette)),
            "expected clash between Palette and QuickOpen: {found_clashes:?}"
        );

        // Resetting restores conflict-free state
        rt.state.edit(ConfigEdit::ResetKeymap);
        let after_reset = clashes(KeyPlatform::current(), |e| e.owner == Owner::Window);
        assert!(after_reset.is_empty());
    }

    #[test]
    fn global_leader_arms_in_session_and_terminal_executes_which_key_and_double_taps() {
        use rune::{Env, Key, Mods, Runtime};

        let mut s = state();
        s.mode = Mode::Session;
        s.draft.focused = true;
        s.term.open = false;

        let mut rt = Runtime::new(s, super::super::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(super::super::on_key);
        rt.frame(0.0);

        let ctrl_space = KeyEvent {
            key: Key::Text(" ".into()),
            mods: Mods {
                ctrl: true,
                cmd: true,
                ..Default::default()
            },
        };
        let plain = |c: char| KeyEvent {
            key: Key::Text(c.to_string()),
            mods: Mods::default(),
        };

        // 1. In Session mode with Composer focused, pressing Leader (Ctrl+Space) arms leader_armed
        rt.key(ctrl_space.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.is_leader_armed());

        // Pressing 'k' opens the Command Palette instead of typing 'k' into the composer draft!
        rt.key(plain('k'));
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.is_leader_armed());
        assert!(rt.state.palette.is_some());
        assert!(rt.state.draft.text.is_empty());
        rt.state.palette = None;
        rt.frame(1.0 / 60.0);

        // 2. Pressing Leader then 'b' toggles the sidebar
        let sidebar_before = rt.state.sidebar.is_open();
        rt.key(ctrl_space.clone());
        rt.key(plain('b'));
        rt.frame(1.0 / 60.0);
        assert_ne!(rt.state.sidebar.is_open(), sidebar_before);

        // 3. Pressing Leader then ' ' (Space) toggles focus to Terminal (opening it)
        assert!(!rt.state.term.open);
        rt.key(ctrl_space.clone());
        rt.key(plain(' '));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.term.open && rt.state.term.sink.focused);

        // 4. Inside focused Terminal, double-tapping Leader forwards literal \x00 to PTY
        rt.state.term.sent_log.borrow_mut().clear();
        rt.key(ctrl_space.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.is_leader_armed());
        rt.key(ctrl_space);
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.is_leader_armed());
        assert_eq!(*rt.state.term.sent_log.borrow(), ["text \0"]);

        // 5. Custom Leader key (Record Leader -> Ctrl+G) works globally and double-taps in Terminal
        rt.state.recording_key = Some(RecordingTarget::Leader);
        let ctrl_g = KeyEvent {
            key: Key::Command('g'),
            mods: Mods {
                ctrl: true,
                cmd: true,
                ..Default::default()
            },
        };
        rt.key(ctrl_g.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.recording_key.is_none());
        assert_eq!(rt.state.cfg.keymap.leader, "ctrl+g");

        // Double-tap custom leader (Ctrl+G) in focused terminal sends key G ctrl=true
        rt.state.term.sent_log.borrow_mut().clear();
        rt.key(ctrl_g.clone());
        rt.frame(1.0 / 60.0);
        assert!(rt.state.is_leader_armed());
        rt.key(ctrl_g.clone());
        rt.frame(1.0 / 60.0);
        assert!(!rt.state.is_leader_armed());
        assert_eq!(*rt.state.term.sent_log.borrow(), ["key G ctrl=true"]);

        // Custom leader (Ctrl+G) followed by Shift+S navigates to Settings
        rt.key(ctrl_g);
        rt.key(KeyEvent {
            key: Key::Text("S".into()),
            mods: Mods {
                shift: true,
                ..Default::default()
            },
        });
        rt.frame(1.0 / 60.0);
        assert_eq!(rt.state.mode, Mode::Settings);
    }
}
