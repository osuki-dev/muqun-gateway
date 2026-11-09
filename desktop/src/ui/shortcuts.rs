//! The keyboard shortcuts sheet (Ctrl/Cmd+/, or `?` when not typing) and the
//! rows Settings → Shortcuts lists: both read `ui/keymap.rs`, grouped, with
//! each chord drawn as keycaps the way this platform writes it.

use rune::{col, el, row, text, El, KeyPlatform, Spring, TextEdit, Transition};
use rune_kit::{Kit, ScrollBar, TextInput};

use super::copy::{tr, tr_ref};
use super::keymap::{self, Entry, Group, Owner};
use super::UiState;
use crate::config::Language;

/// The open sheet: its search.
pub struct Sheet {
    pub query: TextEdit,
}

impl UiState {
    pub fn toggle_keys_sheet(&mut self) {
        self.palette = None;
        self.keys_sheet = match self.keys_sheet {
            Some(_) => None,
            None => {
                let mut query = TextEdit::new("");
                query.focused = true;
                Some(Sheet { query })
            }
        };
    }
}

/// Whether `entry` answers `query` (its words in either language, or keys).
fn matches(entry: &Entry, language: Language, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let caps = entry.keycaps_on(KeyPlatform::current()).concat().join(" ");
    [entry.what(language), entry.en.to_string(), caps]
        .iter()
        .any(|hay| hay.to_lowercase().contains(&query))
}

/// A chord as keycaps: each key a small raised cap in the window's own
/// surface, so it reads as a key and not a tag. Every place that shows a
/// chord in a sentence or list (this sheet, Settings) draws it so.
pub(super) fn keycaps<S: AsRef<str>>(k: &Kit, keys: &[S]) -> El {
    let label = keys.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" ");
    let font = rune::TextStyle {
        line_height: 14.0,
        ..k.ty.small_medium
    };
    let mut line = row().role("group").label(label).gap(4.0).center_y();
    for (i, key) in keys.iter().enumerate() {
        let key = key.as_ref();
        let upper = key.to_uppercase();
        // Only when it maps back: Turkish "ı" would draw as "I", another key.
        let cap = if key.chars().count() == 1 && upper.to_lowercase() == key {
            upper
        } else {
            key.to_string()
        };
        line = line.child(
            row()
                .key(format!("cap{i}"))
                .min_w(22.0)
                .h(22.0)
                .px(6.0)
                .center()
                .radius(k.radius.sm + 1.0)
                .fill(k.c.bg_base)
                .border(1.0, k.c.border_weak)
                .box_shadow(rune::Shadow {
                    x: 0.0,
                    y: 1.0,
                    blur: 0.0,
                    spread: 0.0,
                    color: k.c.border_base,
                    inset: false,
                })
                .child(text(cap).font(font).color(k.c.text_muted))
                .a11y_hidden(),
        );
    }
    line
}

/// The keys of one row: each chord as keycaps, alternatives side by side.
/// A run of chords that differ only in the key (Ctrl+1 … Ctrl+4, the four
/// arrows) reads as one: the modifiers, then the keys.
fn keys(k: &Kit, language: Language, entry: &Entry) -> El {
    let mut chords = entry.keycaps_on(KeyPlatform::current());
    if chords.len() > 2 {
        let mods = chords[0][..chords[0].len() - 1].to_vec();
        if chords.iter().all(|c| c[..c.len() - 1] == mods[..]) {
            let last: Vec<String> = chords.iter().filter_map(|c| c.last().cloned()).collect();
            let digits = last
                .iter()
                .all(|k| k.len() == 1 && k.chars().all(|c| c.is_ascii_digit()));
            let joined = if digits {
                format!("{}–{}", last[0], last[last.len() - 1])
            } else {
                last.join(" ")
            };
            let mut one = mods;
            one.push(joined);
            chords = vec![one];
        }
    }
    let mut line = row().gap(6.0).center_y();
    for (i, caps) in chords.iter().enumerate() {
        if i > 0 {
            line = line.child(
                text(tr_ref(language, "or", "或"))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        }
        line = line.child(keycaps(k, caps));
    }
    line
}

fn entry_row(k: &Kit, language: Language, entry: &Entry, key: String) -> El {
    row()
        .key(key)
        .w_full()
        .gap(12.0)
        .center_y()
        .py(3.0)
        .child(
            text(entry.what(language))
                .font(k.ty.ui)
                .color(k.c.text_base)
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        )
        .child(keys(k, language, entry))
}

/// The terminal's footnote: how the window's shortcuts reach it.
fn terminal_note(language: Language) -> String {
    if KeyPlatform::current() == KeyPlatform::Mac {
        tr(
            language,
            "Every other key goes to the shell; ⌘ shortcuts stay the app's.",
            "其他按键都交给 Shell；⌘ 快捷键仍归应用。",
        )
    } else {
        tr(
            language,
            "Every other key goes to the shell. Add Shift for the app's Ctrl shortcuts (Ctrl+Shift+K for the palette).",
            "其他按键都交给 Shell。应用的 Ctrl 快捷键加上 Shift 即可（Ctrl+Shift+K 打开命令面板）。",
        )
    }
}

/// Each group's title and rows, for `query` (empty: all), skipping empty groups.
pub fn groups(s: &UiState, k: &Kit, query: &str, id: &str) -> Vec<(Group, String, Vec<El>)> {
    let language = s.cfg.general.language;
    let mut out = Vec::new();
    for group in Group::ALL {
        let mut rows: Vec<El> = keymap::ENTRIES
            .iter()
            .enumerate()
            .filter(|(_, e)| e.group == group && matches(e, language, query))
            .map(|(i, e)| entry_row(k, language, e, format!("{id}-{i}")))
            .collect();
        if rows.is_empty() {
            continue;
        }
        if group == Group::Terminal && query.trim().is_empty() {
            rows.push(
                text(terminal_note(language))
                    .key(format!("{id}-terminal-note"))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap()
                    .w_full()
                    .pt(4.0),
            );
        }
        out.push((group, group.title(language), rows));
    }
    out
}

/// What the keymap check found, as sentences: two commands on one chord
/// anywhere, and the window chords the terminal can't lend (one wins).
pub fn conflicts(language: Language) -> (Vec<String>, Vec<String>) {
    let platform = KeyPlatform::current();
    let what = |a| keymap::entry(a).what(language);
    let clashes = [Owner::Window, Owner::Composer, Owner::Terminal]
        .into_iter()
        .flat_map(|owner| {
            keymap::clashes(platform, move |e| {
                e.owner == owner || e.owner == Owner::Window
            })
        })
        .map(|(a, b, keys)| {
            tr(
                language,
                &format!("{keys} is both “{}” and “{}”.", what(a), what(b)),
                &format!("{keys} 同时是“{}”和“{}”。", what(a), what(b)),
            )
        })
        .collect();
    let shadows = keymap::terminal_shadows(platform)
        .into_iter()
        .map(|(winner, lost, keys)| {
            tr(
                language,
                &format!(
                    "In the terminal {keys} is “{}”, so “{}” needs the terminal unfocused.",
                    what(winner),
                    what(lost)
                ),
                &format!(
                    "在终端里 {keys} 是“{}”，所以“{}”要在终端外使用。",
                    what(winner),
                    what(lost)
                ),
            )
        })
        .collect();
    (clashes, shadows)
}

/// Where a text field keeps a key for itself.
pub fn field_notes(language: Language) -> Vec<String> {
    vec![
        tr(
            language,
            "In a text field, Esc leaves the field (or closes its suggestions) first.",
            "在输入框中，Esc 先离开输入框（或关闭其建议列表）。",
        ),
        {
            let jump = keymap::entry(keymap::Action::JumpLatest)
                .keybind()
                .chords()
                .last()
                .map(|c| c.label())
                .unwrap_or_default();
            tr(
                language,
                &format!("In a text field, End moves the caret; {jump} jumps to the latest message once the caret is already at the end."),
                &format!("在输入框中，End 移动光标；光标已在末尾时，{jump} 跳到最新消息。"),
            )
        },
        tr(
            language,
            "j, k, Space, Enter, [ ] and ? work when no text field has the keyboard.",
            "j、k、空格、Enter、[ ] 和 ? 在没有输入框获得键盘时生效。",
        ),
    ]
}

/// Mount the sheet above the window when it is open.
pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let Some(sheet) = s.keys_sheet.as_ref() else {
        return;
    };
    let language = s.cfg.general.language;
    let look = s.look();
    let title = tr_ref(language, "Keyboard shortcuts", "键盘快捷键");
    let field = TextInput::new(&sheet.query)
        .placeholder(tr_ref(language, "Search shortcuts", "搜索快捷键"))
        .label(tr_ref(language, "Search shortcuts", "搜索快捷键"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(None)
        .plain()
        .on_edit(|s: &mut UiState| &mut s.keys_sheet.as_mut().expect("sheet open").query)
        .build(k)
        .key("keys-search")
        .w_full();
    let head = row()
        .key("keys-head")
        .w_full()
        .gap(12.0)
        .center_y()
        .px(20.0)
        .pt(16.0)
        .pb(12.0)
        .child(rune::icon(&super::icons::KEYBOARD, 18.0).color(k.c.icon_muted))
        .child(text(title).font(k.ty.body_medium).color(k.c.text_strong))
        .child(el().grow(1.0))
        .child(
            row()
                .key("keys-search-well")
                .w(240.0)
                .radius(k.radius.md)
                .fill(k.c.text_base.alpha(0.06))
                .child(field),
        );

    let found = groups(s, k, &sheet.query.text, "keys");
    // Two columns: the window's keys on the left, the session's on the right.
    let (mut left, mut right) = (
        col().key("keys-left").w_full().min_w(0.0).gap(16.0),
        col().key("keys-right").w_full().min_w(0.0).gap(16.0),
    );
    let empty = found.is_empty();
    for (group, name, rows) in found {
        let block = col()
            .key(format!("keys-group-{group:?}"))
            .w_full()
            .child(
                text(name)
                    .font(k.ty.small_medium)
                    .color(k.c.text_faint)
                    .pb(4.0),
            )
            .children(rows);
        if matches!(group, Group::Global | Group::Panels | Group::Split) {
            left = left.child(block);
        } else {
            right = right.child(block);
        }
    }
    let body = if empty {
        col()
            .key("keys-body")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .px(20.0)
            .py(24.0)
            .child(
                text(tr(language, "No shortcut matches", "没有匹配的快捷键"))
                    .font(k.ty.ui)
                    .color(k.c.text_faint),
            )
    } else {
        col().key("keys-body").w_full().grow(1.0).min_h(0.0).child(
            row()
                .key("keys-columns")
                .w_full()
                .equal_columns(2)
                .gap(32.0)
                .px(20.0)
                .py(14.0)
                .child(left)
                .child(right),
        )
    }
    .scroll(rune::ScrollSpec {
        stick_bottom: false,
    })
    .scroll_bar(k);

    let foot = row()
        .key("keys-foot")
        .w_full()
        .gap(16.0)
        .px(20.0)
        .py(10.0)
        .child(hint(k, "Esc", tr(language, "close", "关闭")))
        .child(el().grow(1.0))
        .child(
            text(tr(
                language,
                "Settings → Shortcuts lists these too",
                "设置 → 快捷键 中也能查看",
            ))
            .font(k.ty.small)
            .color(k.c.text_faint),
        );

    let panel = col()
        .key("keys-sheet")
        .role("dialog")
        .label(title)
        .w_full()
        .max_w(820.0)
        // A steady height while the search narrows the list.
        .h_full()
        .max_h(720.0)
        .child(head)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(body)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(foot);
    // Denser than the palette's glass: the sheet is text to read.
    let mut panel = look
        .panel(k, panel, crate::theme::look::PANEL_RADIUS)
        .fill(k.c.surface_raised_base.alpha(0.97));
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        panel = panel.box_shadow(shadow);
    }
    let panel = if s.cfg.appearance.reduced_motion {
        panel.transition(Transition::opacity())
    } else {
        panel
            .transition(
                Transition::opacity()
                    .combined(Transition::offset(0.0, 10.0))
                    .combined(Transition::scale(0.97)),
            )
            .motion(Spring::new(0.32, 0.16))
    }
    .modal(true, |s: &mut UiState| s.keys_sheet = None);

    layer.push(
        el().key("keys-scrim")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .fill(k.c.black.alpha(if s.dark { 0.5 } else { 0.26 }))
            .token_motion(k.motion.overlay)
            .a11y_hidden(),
    );
    layer.push(
        col()
            .key("keys-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .items_center()
            .justify_center()
            .px(32.0)
            .py(48.0)
            .child(panel),
    );
}

fn hint(k: &Kit, key: &str, what: String) -> El {
    row()
        .gap(6.0)
        .center_y()
        .child(keycaps(k, &[key]))
        .child(text(what).font(k.ty.small).color(k.c.text_faint))
}
