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
use crate::config::Language;

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
    NewShell,
    ToggleInspector,
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
    TermCopy,
    TermPaste,
    Allow,
    AllowAlways,
    Deny,
    MarkDone,
    CancelTask,
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
    e(A::NewShell, G::Panels, "ctrl+shift+`", "New shell"),
    e(
        A::ToggleInspector,
        G::Panels,
        "mod+alt+b",
        "Show or hide the inspector or detail pane",
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
    pub fn keybind(&self) -> &'static Keybind {
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

    /// What it does, in the owner's language.
    pub fn what(&self, language: Language) -> String {
        crate::ui::copy::t(language, self.en)
    }

    /// Each chord's keycaps on `platform`.
    pub fn keycaps_on(&self, platform: KeyPlatform) -> Vec<Vec<String>> {
        self.keybind()
            .chords_on(platform)
            .iter()
            .map(|c| c.keycaps_on(platform))
            .collect()
    }
}

pub fn entry(action: Action) -> &'static Entry {
    ENTRIES
        .iter()
        .find(|e| e.action == action)
        .expect("every action has a row")
}

/// The chord hints show for `action` ("Ctrl+B", "⌘B").
pub fn label(action: Action) -> String {
    entry(action)
        .keybind()
        .chords()
        .first()
        .map(|c| c.label())
        .unwrap_or_default()
}

/// Every chord of `action`, in the keymap's order (the pane arrows: left,
/// right, up, down).
pub fn labels(action: Action) -> Vec<String> {
    entry(action)
        .keybind()
        .chords()
        .iter()
        .map(|c| c.label())
        .collect()
}

/// A button's tooltip: "Toggle sidebar (Ctrl+B)".
pub fn tip(text: &str, action: Action) -> String {
    format!("{text} ({})", label(action))
}

/// The sidebar toggle's chord as a `&'static str`, as the kit's button takes it.
pub fn sidebar_label() -> &'static str {
    static LABEL: OnceLock<String> = OnceLock::new();
    LABEL.get_or_init(|| label(Action::ToggleSidebar))
}

/// Two rows sharing a chord on `platform`, among rows `pick` keeps.
pub fn clashes(
    platform: KeyPlatform,
    pick: impl Fn(&Entry) -> bool,
) -> Vec<(Action, Action, String)> {
    let rows: Vec<&Entry> = ENTRIES.iter().filter(|e| pick(e)).collect();
    let mut out = Vec::new();
    for (i, a) in rows.iter().enumerate() {
        for b in &rows[i + 1..] {
            for ca in a.keybind().chords_on(platform) {
                if b.keybind()
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

/// Inside the terminal on Windows and Linux, a window chord with Ctrl
/// arrives with Shift added; one that already has Shift arrives as it is.
/// The window chords that land on the same keys there, the first one wins
/// (`(winner, shadowed, keys)`). macOS sends ⌘ chords unchanged.
pub fn terminal_shadows(platform: KeyPlatform) -> Vec<(Action, Action, String)> {
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
        for chord in e.keybind().chords_on(platform) {
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

/// The window's keys (`on_key`): run the first row whose chord this is.
pub fn dispatch(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if crate::term::input::is_toggle(ev, tx.physical_key()) {
        s.toggle_terminal();
        return true;
    }
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
    let Some(found) = ENTRIES
        .iter()
        .filter(|e| e.owner == Owner::Window)
        .find(|e| e.keybind().matches_tx(ev, tx))
    else {
        return false;
    };
    run(s, found.action, ev, tx)
}

/// Keys a composer hears before its editor, which would take them as
/// caret moves: Alt+↑/↓, the split's pane arrows, and Ctrl+End once the
/// caret is already at the end.
pub fn field_key(s: &mut UiState, ev: &KeyEvent, tx: &mut Tx, at_end: bool) -> bool {
    for action in [
        A::PrevSession,
        A::NextSession,
        A::FocusPaneToward,
        A::SwapPaneToward,
    ] {
        if entry(action).keybind().matches_tx(ev, tx) {
            return run(s, action, ev, tx);
        }
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
        A::ToggleTerminal => s.toggle_terminal(),
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
                .position(|c| ev.is_command(*c))
                .unwrap_or(usize::MAX);
            if !(s.in_split() && n < s.tiles.len()) {
                return false;
            }
            s.focus_tile(n);
        }
        A::FocusPaneToward | A::SwapPaneToward => return super::tiles::arrow_key(s, ev),
        A::FocusComposer => match s.mode {
            Mode::Session => s.draft.focused = true,
            Mode::Home => s.start_draft.focused = true,
            _ => return false,
        },
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
    /// The timeline row's detail pane.
    Pane,
    /// In-session timeline search.
    TimelineSearch,
    /// The sidebar's search: cleared, back to the session list.
    Search,
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
    if s.inspect.is_some() {
        return Some(EscStep::Pane);
    }
    if s.timeline_search.active {
        return Some(EscStep::TimelineSearch);
    }
    if s.searching() {
        return Some(EscStep::Search);
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
        EscStep::Pane => s.close_inspect(),
        EscStep::TimelineSearch => s.close_timeline_search(),
        EscStep::Search => s.close_search(),
        EscStep::Page => match last_session(s) {
            Some(_) if s.in_split() => s.mode = Mode::Session,
            Some(asid) => s.open_session(&asid),
            None => s.go(Mode::Home),
        },
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
                // The composer's and the terminal's keys are heard where the
                // window's are too, so each is checked against them.
                let clashes = clashes(platform, |e| e.owner == owner || e.owner == Owner::Window);
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
    }
}
