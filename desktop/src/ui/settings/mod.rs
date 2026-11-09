//! Settings: one page per subject, picked from a nav column inside the main
//! card (an icon rail when the card is narrow).
//!
//! Every page is built from the same few pieces so they read as one system:
//! a fixed header (title, one line of description, the page's own actions)
//! over a scrolling body of [`group`]s, each a quiet header over rows split
//! by hairlines. A [`row`] is a label and helper text on the left and its
//! control on the right, on one column grid; when the card is too narrow
//! for both the control wraps under its label.

mod about;
mod agents;
mod appearance;
mod general;
mod keys;
mod storage;
mod themes;

use rune::{col, el, row as hrow, text, El, TextEdit, Tx};
use rune_kit::shell::SidebarRow;
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, ScrollBar,
    SegmentedControl, Select, SelectAppearance, Switch, TextInput,
};

use super::copy::tr_ref;
use super::{OpenMenu, UiState};
use crate::config::Language;
use crate::model::commands::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    General,
    Appearance,
    Themes,
    Agents,
    Voice,
    Shortcuts,
    Storage,
    About,
}

/// Every page in nav order, with its name.
pub(super) fn tabs(language: Language) -> [(&'static str, SettingsTab); 8] {
    use SettingsTab as T;
    [
        (tr_ref(language, "General", "通用"), T::General),
        (tr_ref(language, "Appearance", "外观"), T::Appearance),
        (tr_ref(language, "Themes", "主题"), T::Themes),
        (tr_ref(language, "Agents", "Agents"), T::Agents),
        (tr_ref(language, "Voice", "语音"), T::Voice),
        (tr_ref(language, "Shortcuts", "快捷键"), T::Shortcuts),
        (tr_ref(language, "Storage", "存储"), T::Storage),
        (tr_ref(language, "About", "关于"), T::About),
    ]
}

impl SettingsTab {
    fn icon(self) -> &'static rune::Icon {
        use super::icons as i;
        match self {
            Self::General => &i::CONTROLS,
            Self::Appearance => &i::APPEARANCE,
            Self::Themes => &i::GRID,
            Self::Agents => &i::AGENT,
            Self::Voice => &super::voice::MIC,
            Self::Shortcuts => &i::KEYBOARD,
            Self::Storage => &i::STORAGE,
            Self::About => &i::INFORMATION,
        }
    }

    /// The page's one line under its title.
    fn description(self, language: Language) -> &'static str {
        match self {
            Self::General => tr_ref(
                language,
                "Language, where new sessions start, and notifications.",
                "界面语言、新会话的起点与通知。",
            ),
            Self::Appearance => tr_ref(
                language,
                "Light or dark, text size, density and motion.",
                "浅色或深色、文字大小、密度与动效。",
            ),
            Self::Themes => tr_ref(
                language,
                "Restyle the whole window: colours, artwork and type.",
                "更换整个窗口的配色、插图与字体。",
            ),
            Self::Agents => tr_ref(
                language,
                "The coding agents found on this computer. Changes save as you type.",
                "在这台电脑上发现的编程 Agents。修改随输入保存。",
            ),
            Self::Voice => super::voice::subtitle(language),
            Self::Shortcuts => tr_ref(
                language,
                "Every key the window answers to.",
                "窗口响应的所有按键。",
            ),
            Self::Storage => tr_ref(
                language,
                "What Muqun keeps on this computer.",
                "Muqun 在这台电脑上保存的内容。",
            ),
            Self::About => tr_ref(
                language,
                "Version, connection, links and licences.",
                "版本、连接、链接与许可。",
            ),
        }
    }
}

/// Open `tab`, asking for what it shows.
pub(super) fn open(s: &mut UiState, tab: SettingsTab) {
    s.settings_tab = tab;
    s.menu = None;
    match tab {
        SettingsTab::Themes => s.send(Command::ThemeRefresh),
        SettingsTab::Storage => s.send(Command::ScanStorage),
        _ => {}
    }
}

// ---- the grid --------------------------------------------------------------------------

/// The body column's width; Themes takes more for its grid.
const BODY_W: f32 = 720.0;
const WIDE_BODY_W: f32 = 1040.0;
/// The nav column, and the rail it folds into on a narrow card.
const NAV_W: f32 = 196.0;
const RAIL_W: f32 = 56.0;
/// Under this card width the nav folds into a rail.
const FOLD_AT: f32 = 820.0;
/// The page's side padding.
const PAD_X: f32 = 36.0;
/// The width text fields and selects share, so controls line up.
pub(super) const CONTROL_W: f32 = 280.0;
/// A label column never gets narrower than this before its control wraps.
const LABEL_MIN_W: f32 = 240.0;

/// A settings row: `title` and `detail` on the left, `control` at the
/// trailing edge. The control wraps under the label when both don't fit.
pub(super) fn row(k: &Kit, key: &str, title: &str, detail: &str, control: El) -> El {
    hrow()
        .key(format!("set-{key}"))
        .w_full()
        .min_h(52.0)
        .py(10.0)
        .gap(16.0)
        .center_y()
        .flex_wrap()
        .child(label(k, title, detail))
        .child(control)
}

/// A row whose control sits under the label at full width (a long path, a
/// note), or a row of content of its own.
pub(super) fn row_below(k: &Kit, key: &str, title: &str, detail: &str, below: El) -> El {
    col()
        .key(format!("set-{key}"))
        .w_full()
        .py(10.0)
        .gap(8.0)
        .child(label(k, title, detail))
        .child(below)
}

fn label(k: &Kit, title: &str, detail: &str) -> El {
    let mut copy = col()
        .key("label")
        .w(LABEL_MIN_W)
        .min_w(LABEL_MIN_W)
        .grow(1.0)
        .gap(2.0)
        .child(text(title.to_string()).font(k.ty.ui).color(k.c.text_base));
    if !detail.is_empty() {
        copy = copy.child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .max_w(460.0),
        );
    }
    copy
}

/// A group of rows under a quiet header, split by hairlines.
pub(super) fn group(k: &Kit, key: &str, title: &str, rows: Vec<El>) -> El {
    group_with(k, key, group_head(k, title, None), rows)
}

/// [`group`] with something at the header's trailing end.
pub(super) fn group_action(k: &Kit, key: &str, title: &str, action: El, rows: Vec<El>) -> El {
    group_with(k, key, group_head(k, title, Some(action)), rows)
}

pub(super) fn group_head(k: &Kit, title: &str, trailing: Option<El>) -> El {
    hrow()
        .key("head")
        .w_full()
        .min_h(24.0)
        .gap(12.0)
        .center_y()
        .child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_muted)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .truncate(),
        )
        .children(trailing)
}

/// A group with any header (an agent's, say).
pub(super) fn group_with(k: &Kit, key: &str, head: El, rows: Vec<El>) -> El {
    let mut body = col().key("rows").w_full();
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            body = body.child(hairline(k, i));
        }
        body = body.child(r);
    }
    col()
        .key(key.to_string())
        .w_full()
        .gap(6.0)
        .child(head)
        .child(hairline(k, 0))
        .child(body)
}

fn hairline(k: &Kit, i: usize) -> El {
    el().key(format!("rule-{i}"))
        .w_full()
        .h(1.0)
        .fill(k.c.border_weak)
        .a11y_hidden()
}

/// A line of status under a row: what was saved, what went wrong.
#[derive(Clone, Copy)]
pub(super) enum Tone {
    Quiet,
    Good,
    Bad,
}

pub(super) fn status(k: &Kit, key: &str, tone: Tone, words: impl Into<String>) -> El {
    let color = match tone {
        Tone::Quiet => k.c.text_muted,
        Tone::Good => k.c.success.icon,
        Tone::Bad => k.c.danger.fg,
    };
    hrow()
        .key(key.to_string())
        .gap(6.0)
        .center_y()
        .child(el().size(6.0, 6.0).radius(3.0).fill(color).a11y_hidden())
        .child(
            text(words.into())
                .font(k.ty.small)
                .color(color)
                .wrap()
                .shrink()
                .min_w(0.0),
        )
}

// ---- controls ------------------------------------------------------------------------

pub(super) fn choice(
    k: &Kit,
    key: &str,
    labels: &[&str],
    at: usize,
    pick: impl Fn(&mut UiState, usize) + Clone + 'static,
) -> El {
    SegmentedControl::new(labels, Some(at))
        .fit()
        .label(key.to_string())
        .on_pick(move |s: &mut UiState, i, _: &mut Tx| pick(s, i))
        .build(k)
        .key(format!("seg-{key}"))
}

pub(super) fn flag(
    k: &Kit,
    key: &str,
    on: bool,
    label: &str,
    f: impl Fn(&mut UiState) + 'static,
) -> El {
    Switch::new(on)
        .label(label)
        .hide_label()
        .on_toggle(f)
        .build(k)
        .key(format!("sw-{key}"))
}

/// A dropdown of `options` (shown as `labels`), for a choice too long for
/// a segmented control.
pub(super) fn select(
    s: &UiState,
    k: &Kit,
    layer: &mut Vec<El>,
    key: &str,
    labels: &[&str],
    at: usize,
    pick: impl Fn(&mut UiState, usize) + Clone + 'static,
) -> El {
    let menu = OpenMenu::Setting(key.to_string());
    let open = s.menu.as_ref() == Some(&menu);
    let owned: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    Select::new(format!("sel-{key}"), labels)
        .appearance(SelectAppearance::Field)
        .width(CONTROL_W)
        .current(labels.get(at).copied())
        .label(key.to_string())
        .open(open)
        .on_toggle(move |s: &mut UiState| {
            s.menu = if s.menu.as_ref() == Some(&menu) {
                None
            } else {
                Some(menu.clone())
            };
        })
        .on_close(|s: &mut UiState| s.menu = None)
        .on_choose(move |s: &mut UiState, chosen: &str, _: &mut Tx| {
            s.menu = None;
            if let Some(i) = owned.iter().position(|o| o == chosen) {
                pick(s, i);
            }
        })
        // Keeps the kit's own key: the listbox anchors to its trigger by it.
        .build(k, layer)
}

/// A text field at the shared control width.
pub(super) fn field(input: TextInput, k: &Kit, key: &str) -> El {
    input
        .width(Some(CONTROL_W))
        .build(k)
        .key(format!("in-{key}"))
}

pub(super) fn link_button(k: &Kit, key: &str, label: &str, url: &'static str) -> El {
    Button::new(label)
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Small)
        .icon(&super::icons::OPEN_EXTERNAL)
        .help(url)
        .on_press_tx(move |_: &mut UiState, tx: &mut Tx| {
            let _ = tx.open_url(url);
        })
        .build(k)
        .key(key.to_string())
}

/// A quiet card standing in for a page with nothing to show yet.
pub(super) fn empty(k: &Kit, key: &str, title: &str, detail: &str) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .py(48.0)
        .gap(6.0)
        .items_center()
        .child(
            text(title.to_string())
                .font(k.ty.ui_medium)
                .color(k.c.text_base),
        )
        .child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .max_w(420.0),
        )
}

// ---- the page --------------------------------------------------------------------------

/// The nav: every page as a sidebar row (the app's own sidebar look), or as
/// icons in a rail on a narrow card.
fn nav(s: &UiState, k: &Kit, rail: bool) -> El {
    let language = s.cfg.general.language;
    let mut list = col()
        .key("settings-nav")
        .w(if rail { RAIL_W } else { NAV_W })
        .h_full()
        .pt(26.0)
        .px(if rail { 10.0 } else { 12.0 })
        .gap(2.0)
        .role("tablist")
        .label(tr_ref(language, "Settings", "设置"));
    for (i, (name, tab)) in tabs(language).into_iter().enumerate() {
        // A breath between what the window looks like, what it runs, and
        // what it keeps.
        if i == 3 || i == 6 {
            list = list.child(el().key(format!("nav-gap-{i}")).h(10.0));
        }
        let selected = s.settings_tab == tab;
        let item = if rail {
            IconButton::new(tab.icon())
                .variant(if selected {
                    IconButtonVariant::Neutral
                } else {
                    IconButtonVariant::Ghost
                })
                .control_size(ControlSize::Large)
                .label(name)
                .help(name)
                .on_press(move |s: &mut UiState| open(s, tab))
                .build(k)
        } else {
            SidebarRow::new(name)
                .icon(tab.icon())
                .selected(selected)
                .on_select(move |s: &mut UiState, _: &mut Tx| open(s, tab))
                .build(k)
        };
        list = list.child(item.key(format!("settings-nav-{tab:?}")));
    }
    list
}

/// The page's fixed header: its title, its line, and its own actions.
fn header(k: &Kit, tab: SettingsTab, language: Language, actions: Vec<El>, w: f32) -> El {
    let name = tabs(language)
        .into_iter()
        .find(|(_, t)| *t == tab)
        .map_or("", |(n, _)| n);
    col()
        .key("settings-head")
        .w_full()
        .max_w(w)
        .px(PAD_X)
        .pt(28.0)
        .pb(14.0)
        .gap(4.0)
        .child(
            hrow()
                .key("settings-head-line")
                .w_full()
                .min_h(30.0)
                .gap(12.0)
                .center_y()
                .child(
                    text(name.to_string())
                        .key("page-title")
                        .font(rune::TextStyle {
                            size: 22.0,
                            line_height: 28.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base)
                        .grow(1.0)
                        .shrink()
                        .truncate(),
                )
                .children(actions),
        )
        .child(
            text(tab.description(language).to_string())
                .key("page-subtitle")
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        )
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    let tab = s.settings_tab;
    let card_w = rune::memo::layout_with("main", |r| r[2]).unwrap_or(1080.0);
    let rail = card_w < FOLD_AT;
    let w = if tab == SettingsTab::Themes {
        WIDE_BODY_W
    } else {
        BODY_W
    };
    let (actions, body): (Vec<El>, Vec<El>) = match tab {
        SettingsTab::General => (Vec::new(), general::page(s, k, layer)),
        SettingsTab::Appearance => (Vec::new(), appearance::page(s, k)),
        SettingsTab::Themes => (themes::actions(s, k), themes::page(s, k)),
        SettingsTab::Agents => (agents::actions(s, k), agents::page(s, k, layer)),
        SettingsTab::Voice => (Vec::new(), super::voice::settings(s, k, layer)),
        SettingsTab::Shortcuts => (Vec::new(), keys::page(s, k)),
        SettingsTab::Storage => (Vec::new(), storage::page(s, k)),
        SettingsTab::About => (Vec::new(), about::page(s, k)),
    };
    let mut page = col()
        .key(format!("settings-body-{tab:?}"))
        .w_full()
        .max_w(w)
        .px(PAD_X)
        .pt(12.0)
        .pb(48.0)
        .gap(36.0);
    for (i, child) in body.into_iter().enumerate() {
        page = page.child(look.arrive(child, i.min(4)));
    }
    let content = col()
        .key("settings-content")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .items_center()
        .child(header(k, tab, language, actions, w))
        .child(
            // The body scrolls under the fixed header, fading at its top.
            col()
                .key(format!("settings-scroll-{tab:?}"))
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .items_center()
                .clip_fade(0.0, 14.0, 0.0, 0.0)
                .child(page)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        );
    // Settings has no toolbar: like Home, the stage rises under the card's
    // empty toolbar row, so the nav and page start at the card's top edge.
    let lift = rune_kit::shell::TOOLBAR_HEIGHT;
    let card_h = rune::memo::layout_with("main-pane", |r| r[3]);
    let mut stage = col()
        .key("settings-stage")
        .w_full()
        .min_h(0.0)
        .items_center();
    stage = match card_h {
        Some(h) => stage.h(h).offset(0.0, -lift),
        None => stage.h_full(),
    };
    if look.glass {
        stage = stage.child(look.light(
            "settings-light",
            [0.55, -0.1],
            [720.0, 340.0],
            k.c.bg_accent,
        ));
    }
    // On a very wide card the nav and page stay together, centred.
    stage.child(
        hrow()
            .key("settings")
            .w_full()
            .max_w(NAV_W + WIDE_BODY_W + 80.0)
            .h_full()
            .min_h(0.0)
            .child(nav(s, k, rail))
            .child(content),
    )
}

/// The default `TextEdit` for a stored value, or the one being typed.
pub(super) fn edit_of(s: &UiState, slot: &str, stored: impl FnOnce() -> String) -> TextEdit {
    s.agent_inputs
        .get(slot)
        .cloned()
        .unwrap_or_else(|| TextEdit::new(stored()))
}
