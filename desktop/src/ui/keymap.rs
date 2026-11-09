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

use super::copy::tr;
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
    PrevSession,
    NextSession,
    AddSplit,
    RemoveSplit,
    FocusPane,
    FocusPaneToward,
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
        let (en, zh) = match self {
            Group::Global => ("Global", "全局"),
            Group::Panels => ("Panels", "面板"),
            Group::Session => ("Session", "会话"),
            Group::Timeline => ("Timeline", "时间线"),
            Group::Split => ("Split view", "分屏"),
            Group::Terminal => ("Terminal", "终端"),
            Group::Lists => ("Inbox and Tasks", "收件箱与任务"),
        };
        tr(language, en, zh)
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
    pub zh: &'static str,
}

const fn e(
    action: Action,
    group: Group,
    spec: &'static str,
    en: &'static str,
    zh: &'static str,
) -> Entry {
    Entry {
        action,
        group,
        owner: Owner::Window,
        spec,
        mac: None,
        en,
        zh,
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
    e(
        A::Shortcuts,
        G::Global,
        "mod+/, ?",
        "Keyboard shortcuts",
        "键盘快捷键",
    ),
    e(
        A::Palette,
        G::Global,
        "mod+k, mod+shift+p",
        "Command palette",
        "命令面板",
    ),
    e(
        A::QuickOpen,
        G::Global,
        "mod+p",
        "Go to session…",
        "跳转到会话…",
    ),
    e(A::NewSession, G::Global, "mod+n", "New session", "新建会话"),
    e(A::Home, G::Global, "mod+shift+h", "Home", "首页"),
    e(A::Inbox, G::Global, "mod+shift+i", "Inbox", "收件箱"),
    e(A::Tasks, G::Global, "mod+shift+t", "Tasks", "任务"),
    e(A::Settings, G::Global, "mod+comma", "Settings", "设置"),
    e(
        A::Back,
        G::Global,
        "escape",
        "Close the innermost popup, then back to the session",
        "关闭最上层弹窗，再返回会话",
    ),
    e(
        A::ToggleSidebar,
        G::Panels,
        "mod+b",
        "Show or hide the sidebar",
        "显示或隐藏侧边栏",
    ),
    e(
        A::ToggleTerminal,
        G::Panels,
        "ctrl+`",
        "Show or hide the terminal",
        "显示或隐藏终端",
    ),
    e(
        A::NewShell,
        G::Panels,
        "ctrl+shift+`",
        "New shell",
        "新的 Shell",
    ),
    e(
        A::ToggleInspector,
        G::Panels,
        "mod+alt+b",
        "Show or hide the inspector or detail pane",
        "显示或隐藏检查器或详情窗格",
    ),
    e(
        A::FocusSearch,
        G::Panels,
        "mod+f",
        "Search sessions",
        "搜索会话",
    ),
    e(
        A::SearchMessages,
        G::Panels,
        "mod+shift+f",
        "Search in messages",
        "搜索消息内容",
    ),
    e(
        A::PrevSession,
        G::Panels,
        "alt+up",
        "Previous session in the sidebar",
        "侧边栏上一个会话",
    ),
    e(
        A::NextSession,
        G::Panels,
        "alt+down",
        "Next session in the sidebar",
        "侧边栏下一个会话",
    ),
    e(
        A::FocusComposer,
        G::Session,
        "mod+l",
        "Focus the message field",
        "聚焦消息输入框",
    ),
    owned(
        e(
            A::Send,
            G::Session,
            "mod+enter",
            "Send (steer while the agent works)",
            "发送（Agent 工作时插话）",
        ),
        Owner::Composer,
    ),
    owned(
        e(
            A::Queue,
            G::Session,
            "alt+enter",
            "Queue after the running turn",
            "排到当前回合之后",
        ),
        Owner::Composer,
    ),
    e(
        A::Stop,
        G::Session,
        "mod+period",
        "Stop the agent",
        "停止 Agent",
    ),
    e(
        A::ModelPicker,
        G::Session,
        "mod+shift+m",
        "Choose the model",
        "选择模型",
    ),
    e(
        A::Dictation,
        G::Session,
        "mod+shift+space",
        "Start or finish dictation",
        "开始 / 结束语音输入",
    ),
    e(
        A::JumpLatest,
        G::Session,
        "end, mod+end",
        "Jump to the latest message",
        "跳到最新消息",
    ),
    e(A::NextRow, G::Timeline, "j", "Next row", "下一行"),
    e(A::PrevRow, G::Timeline, "k", "Previous row", "上一行"),
    e(
        A::ToggleRow,
        G::Timeline,
        "space",
        "Open or fold the row",
        "展开或折叠该行",
    ),
    e(
        A::OpenRow,
        G::Timeline,
        "enter",
        "Show the row's details",
        "查看该行详情",
    ),
    e(
        A::NextMessage,
        G::Timeline,
        "]",
        "Next message",
        "下一条消息",
    ),
    e(
        A::PrevMessage,
        G::Timeline,
        "[",
        "Previous message",
        "上一条消息",
    ),
    e(A::Allow, G::Lists, "a", "Allow", "允许"),
    e(
        A::AllowAlways,
        G::Lists,
        "shift+a",
        "Always allow",
        "始终允许",
    ),
    e(A::Deny, G::Lists, "d", "Deny", "拒绝"),
    e(
        A::MarkDone,
        G::Lists,
        "m",
        "Mark the paused task done",
        "将暂停的任务标记完成",
    ),
    e(
        A::CancelTask,
        G::Lists,
        "x",
        "Cancel the task (press twice)",
        "取消任务（按两次）",
    ),
    e(
        A::AddSplit,
        G::Split,
        "mod+\\",
        "Add a session to split view",
        "加入分屏",
    ),
    e(
        A::RemoveSplit,
        G::Split,
        "mod+w",
        "Remove the pane from split view",
        "把窗格移出分屏",
    ),
    e(
        A::FocusPane,
        G::Split,
        "mod+1, mod+2, mod+3, mod+4",
        "Focus pane 1–4",
        "聚焦第 1–4 个窗格",
    ),
    e(
        A::FocusPaneToward,
        G::Split,
        "mod+alt+left, mod+alt+right, mod+alt+up, mod+alt+down",
        "Focus the pane that way",
        "聚焦该方向的窗格",
    ),
    owned(
        on_mac(
            e(A::TermCopy, G::Terminal, "ctrl+shift+c", "Copy", "复制"),
            "mod+c",
        ),
        Owner::Terminal,
    ),
    owned(
        on_mac(
            e(
                A::TermPaste,
                G::Terminal,
                "ctrl+shift+v, shift+insert",
                "Paste",
                "粘贴",
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
        tr(language, self.en, self.zh)
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
    for action in [A::PrevSession, A::NextSession, A::FocusPaneToward] {
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
        A::FocusSearch => s.focus_search(false),
        A::SearchMessages => s.focus_search(true),
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
        A::FocusPaneToward => return super::tiles::arrow_key(s, ev),
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
    /// A page other than the session: back to the last session (or Home).
    Page,
}

/// The step Esc would take now; `None` leaves it alone.
pub fn esc_step(s: &UiState) -> Option<EscStep> {
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
            if s.voice.is_some() {
                s.cancel_voice();
            } else if s.viewer.is_some() {
                s.close_viewer();
            } else if s.zoom.take().is_none() {
                s.close_detail();
            }
        }
        EscStep::Pane => s.close_inspect(),
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
        // message search's, so Ctrl+F can't. Nothing else collides.
        for platform in [KeyPlatform::Windows, KeyPlatform::Linux] {
            assert_eq!(
                terminal_shadows(platform),
                vec![
                    (A::Palette, A::QuickOpen, "Ctrl+Shift+P".to_string()),
                    (
                        A::SearchMessages,
                        A::FocusSearch,
                        "Ctrl+Shift+F".to_string()
                    ),
                ]
            );
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
                cmd: ctrl,
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
        let mut steps = Vec::new();
        while let Some(step) = esc_step(&s) {
            steps.push(step);
            assert!(escape(&mut s));
        }
        assert_eq!(
            steps,
            [EscStep::Sheet, EscStep::Popup, EscStep::Pane, EscStep::Page]
        );
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
