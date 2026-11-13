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
pub use themes::ThemeTab;

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
        (tr_ref(language, "General"), T::General),
        (tr_ref(language, "Appearance"), T::Appearance),
        (tr_ref(language, "Themes"), T::Themes),
        (tr_ref(language, "Agents"), T::Agents),
        (tr_ref(language, "Voice"), T::Voice),
        (tr_ref(language, "Shortcuts"), T::Shortcuts),
        (tr_ref(language, "Storage"), T::Storage),
        (tr_ref(language, "About"), T::About),
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
            ),
            Self::Appearance => tr_ref(language, "Light or dark, text size, density and motion."),
            Self::Themes => tr_ref(
                language,
                "Restyle the whole window: colours, artwork and type.",
            ),
            Self::Agents => tr_ref(
                language,
                "The coding agents found on this computer. Changes save as you type.",
            ),
            Self::Voice => super::voice::subtitle(language),
            Self::Shortcuts => tr_ref(language, "Every key the window answers to."),
            Self::Storage => tr_ref(language, "What Muqun keeps on this computer."),
            Self::About => tr_ref(language, "Version, connection, links and licences."),
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

/// The page column's widest, padding included: past it the nav and page
/// stay together as one composition centred in the card. Themes alone
/// runs on into the spare width, for more columns of pictures.
const BODY_MAX: f32 = 1100.0;
/// The nav column, and the rail it folds into on a narrow card.
const NAV_W: f32 = 196.0;
const RAIL_W: f32 = 56.0;
/// Under this card width the nav folds into a rail.
const FOLD_AT: f32 = 820.0;
/// The page's side padding.
const PAD_X: f32 = 36.0;
/// The control column's range: it takes this share of the page's inner
/// width, so a wide window gives fields room for a whole URL.
const CONTROL_MIN: f32 = 300.0;
const CONTROL_MAX: f32 = 440.0;
const CONTROL_SHARE: f32 = 0.43;
/// Between the label column and the control column.
const COLUMN_GAP: f32 = 40.0;
/// A label column never gets narrower than this before its control wraps.
const LABEL_MIN_W: f32 = 240.0;
/// One height for every control in a row: fields, selects, segmented
/// controls and buttons.
pub(super) const CONTROL_SIZE: ControlSize = ControlSize::Large;

thread_local! {
    /// The page's inner width this frame, set by [`view`] before it builds
    /// the page.
    static INNER_W: std::cell::Cell<f32> = const { std::cell::Cell::new(BODY_MAX - 2.0 * PAD_X) };
}

/// The width fields, selects and inline field+button pairs share, so
/// controls line up; the whole page width once the control has wrapped
/// under its label.
pub(super) fn control_w() -> f32 {
    if stacked() {
        INNER_W.get().max(0.0)
    } else {
        column_w()
    }
}

/// The control column beside the labels.
fn column_w() -> f32 {
    (INNER_W.get() * CONTROL_SHARE).clamp(CONTROL_MIN, CONTROL_MAX)
}

/// Whether the page is too narrow for a label column beside the control
/// column, so wide controls wrap under their labels.
fn stacked() -> bool {
    INNER_W.get() < LABEL_MIN_W + COLUMN_GAP + column_w()
}

/// A settings row: `title` and `detail` on the left, `control` at the
/// trailing edge. The control wraps under the label when both don't fit.
pub(super) fn row(k: &Kit, key: &str, title: &str, detail: &str, control: El) -> El {
    row_with(k, key, label(k, title, detail), control)
}

/// [`row`] with a label of its own (a status line under the helper text).
pub(super) fn row_with(_k: &Kit, key: &str, label: El, control: El) -> El {
    let r = hrow()
        .key(format!("set-{key}"))
        .w_full()
        .min_h(56.0)
        .py(12.0)
        .center_y();
    // Side by side every control fits its column; stacked, a field takes
    // the whole width under its label while a switch still fits beside it.
    let r = if stacked() {
        r.gap(10.0).flex_wrap()
    } else {
        r.gap(COLUMN_GAP)
    };
    r.child(label).child(control)
}

/// A row's title over its helper text, taking what the control leaves.
pub(super) fn label(k: &Kit, title: &str, detail: &str) -> El {
    let mut copy = col()
        .key("label")
        .w(LABEL_MIN_W)
        .min_w(LABEL_MIN_W)
        .grow(1.0)
        .shrink()
        .gap(3.0)
        .child(text(title.to_string()).font(k.ty.ui).color(k.c.text_base));
    if !detail.is_empty() {
        copy = copy.child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        );
    }
    copy
}

/// A group of rows under a quiet header, split by hairlines.
pub(super) fn group(k: &Kit, key: &str, title: &str, rows: Vec<El>) -> El {
    group_with(k, key, group_head(k, title, None), rows)
}

/// [`group`] with something at the header's trailing end.
#[allow(dead_code)]
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
        .child(head.pb(8.0))
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
        .width(control_w())
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
pub(super) fn field(input: TextInput, edit: &TextEdit, k: &Kit, key: &str) -> El {
    field_at(input, edit, k, key, control_w())
}

/// A text field `w` wide. A field never wraps: while it is not being
/// edited a value too long for it is shown cut in the middle, with the
/// whole of it in a tooltip; editing scrolls it sideways under the caret.
pub(super) fn field_at(input: TextInput, edit: &TextEdit, k: &Kit, key: &str, w: f32) -> El {
    let built = input.width(Some(w)).build(k).key(format!("in-{key}"));
    // The kit's field: 8 points in from each side, its value in `ui_compact`.
    const INSET: f32 = 8.0;
    let value = edit.text.as_str();
    let long = !edit.focused
        && !edit.is_secure()
        && rune::text::measure(value, &[], k.ty.ui_compact, None).0 > w - 2.0 * INSET - 2.0;
    if !long {
        return built;
    }
    el().key(format!("field-{key}"))
        .w(w)
        .help(value.to_string())
        .child(built)
        .child(
            hrow()
                .key("cut")
                .pin(Some(INSET), Some(1.0), Some(INSET), Some(1.0))
                .center_y()
                .fill(k.c.bg_base)
                .child(
                    text(value.to_string())
                        .font(k.ty.ui_compact)
                        .color(k.c.text_base)
                        .truncate_middle()
                        .shrink()
                        .min_w(0.0),
                )
                .a11y_hidden(),
        )
}

/// A field with its button beside it, the pair as wide as any control.
pub(super) fn field_button(
    input: TextInput,
    edit: &TextEdit,
    k: &Kit,
    key: &str,
    button: Button,
    button_w: f32,
) -> El {
    let gap = 8.0;
    hrow()
        .key(format!("pair-{key}"))
        .w(control_w())
        .gap(gap)
        .center_y()
        .child(field_at(input, edit, k, key, control_w() - button_w - gap))
        .child(
            button
                .control_size(CONTROL_SIZE)
                .build(k)
                .key(format!("btn-{key}"))
                .w(button_w),
        )
}

/// How wide a [`button`] labelled `label` draws, for laying out a pair.
pub(super) fn button_w(k: &Kit, label: &str) -> f32 {
    (rune::text::measure(label, &[], k.ty.ui_medium, None).0 + 28.0).max(76.0)
}

/// A button in a row's control column (or a page's header), at the one
/// control height.
pub(super) fn button(label: &str) -> Button {
    Button::new(label)
        .variant(ButtonVariant::Neutral)
        .control_size(CONTROL_SIZE)
}

pub(super) fn link_button(k: &Kit, key: &str, label: &str, url: &'static str) -> El {
    button(label)
        .icon(&super::icons::OPEN_EXTERNAL)
        .help(url)
        .on_press_tx(move |_: &mut UiState, tx: &mut Tx| {
            let _ = tx.open_url(url);
        })
        .build(k)
        .key(key.to_string())
}

/// A long single-line value (a path, a command): one line, cut in the
/// middle when it doesn't fit, the whole of it in a tooltip.
pub(super) fn long_text(k: &Kit, key: &str, value: String, font: rune::TextStyle) -> El {
    text(value.clone())
        .key(key.to_string())
        .font(font)
        .color(k.c.text_muted)
        .truncate_middle()
        .shrink()
        .min_w(0.0)
        .help_when_truncated(value)
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
        .label(tr_ref(language, "Settings"));
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

/// The page's fixed header: its title, its line, its own actions, and an optional sub-bar.
fn header(
    k: &Kit,
    tab: SettingsTab,
    language: Language,
    actions: Vec<El>,
    sub: Option<El>,
    max_w: f32,
) -> El {
    let name = tabs(language)
        .into_iter()
        .find(|(_, t)| *t == tab)
        .map_or("", |(n, _)| n);
    let mut head = col()
        .key("settings-head")
        .w_full()
        .max_w(max_w)
        .px(PAD_X)
        .pt(28.0)
        .pb(18.0)
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
        );
    if let Some(sub) = sub {
        head = head.child(el().key("head-sub-gap").h(10.0)).child(sub);
    }
    head
}

/// Where the nav and page sit in a card `card_w` wide: the margin before
/// the nav, the one after the page, and the page column's widest. The pair
/// is centred as one; Themes keeps the nav where every page has it and
/// runs on into the card's spare width.
fn frame(card_w: f32, nav_w: f32, tab: SettingsTab) -> (f32, f32, f32) {
    let margin = ((card_w - nav_w - BODY_MAX) / 2.0).max(0.0);
    if tab == SettingsTab::Themes {
        let trailing = margin.min(PAD_X);
        (margin, trailing, card_w - margin - trailing - nav_w)
    } else {
        (margin, margin, BODY_MAX)
    }
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let look = s.look();
    let tab = s.settings_tab;
    let card_w = rune::memo::layout_with("main", |r| r[2]).unwrap_or(1080.0);
    let rail = card_w < FOLD_AT;
    let nav_w = if rail { RAIL_W } else { NAV_W };
    let (lead, trail, max_w) = frame(card_w, nav_w, tab);
    INNER_W.set((card_w - lead - trail - nav_w).min(max_w) - 2.0 * PAD_X);
    let (actions, sub, body): (Vec<El>, Option<El>, Vec<El>) = match tab {
        SettingsTab::General => (Vec::new(), None, general::page(s, k, layer)),
        SettingsTab::Appearance => (Vec::new(), None, appearance::page(s, k)),
        SettingsTab::Themes => (
            themes::actions(s, k),
            Some(themes::sub_header(s, k)),
            themes::page(s, k),
        ),
        SettingsTab::Agents => (agents::actions(s, k), None, agents::page(s, k, layer)),
        SettingsTab::Voice => (Vec::new(), None, super::voice::settings(s, k, layer)),
        SettingsTab::Shortcuts => (keys::actions(s, k), None, keys::page(s, k)),
        SettingsTab::Storage => (Vec::new(), None, storage::page(s, k)),
        SettingsTab::About => (Vec::new(), None, about::page(s, k)),
    };
    let mut page = col()
        .key(format!("settings-body-{tab:?}"))
        .w_full()
        .max_w(max_w)
        .px(PAD_X)
        .pt(14.0)
        .pb(56.0)
        .gap(40.0);
    for (i, child) in body.into_iter().enumerate() {
        page = page.child(look.arrive(child, i.min(4)));
    }
    // The page starts right after the nav, not centred in what is left.
    let scroll_key = if tab == SettingsTab::Themes {
        format!("settings-scroll-Themes-{:?}", s.theme_tab)
    } else {
        format!("settings-scroll-{tab:?}")
    };
    let content = col()
        .key("settings-content")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .child(header(k, tab, language, actions, sub, max_w))
        .child(
            // The body scrolls under the fixed header, fading at its top.
            col()
                .key(scroll_key)
                .w_full()
                .grow(1.0)
                .min_h(0.0)
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
    let mut stage = col().key("settings-stage").w_full().min_h(0.0);
    stage = match card_h {
        Some(h) => stage.h(h).offset(0.0, -lift),
        None => stage.h_full(),
    };
    stage.child(
        hrow()
            .key("settings")
            .w_full()
            .h_full()
            .min_h(0.0)
            .pl(lead)
            .pr(trail)
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
