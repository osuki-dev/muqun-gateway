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
pub(super) fn entry_keys(
    k: &Kit,
    language: Language,
    entry: &Entry,
    cfg: Option<&crate::config::KeymapConfig>,
) -> El {
    let mut chords = keymap::keycaps_for(entry, KeyPlatform::current(), cfg);
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
                text(tr_ref(language, "or"))
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            );
        }
        line = line.child(keycaps(k, caps));
    }
    line
}

fn keys(k: &Kit, language: Language, entry: &Entry) -> El {
    entry_keys(k, language, entry, None)
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
        )
    } else {
        tr(
            language,
            "Every other key goes to the shell. Add Shift for the app's Ctrl shortcuts (Ctrl+Shift+K for the palette)."
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
                if owner == Owner::Terminal {
                    e.owner == Owner::Terminal
                } else {
                    e.owner == owner || e.owner == Owner::Window
                }
            })
        })
        .map(|(a, b, keys)| {
            tr(
                language,
                &format!("{keys} is both “{}” and “{}”.", what(a), what(b)),
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
        ),
        {
            let jump = keymap::entry(keymap::Action::JumpLatest)
                .keybind()
                .chords()
                .last()
                .map(|c| c.label())
                .unwrap_or_default();
            tr(language, &format!("In a text field, End moves the caret; {jump} jumps to the latest message once the caret is already at the end."))
        },
        tr(
            language,
            "j, k, Space, Enter, [ ] and ? work when no text field has the keyboard.",
        ),
    ]
}

/// Mount the sheet and/or the Which-Key overlay above the window when open.
pub fn mount(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    if s.is_leader_armed() {
        mount_which_key(s, k, layer);
    }
    let Some(sheet) = s.keys_sheet.as_ref() else {
        return;
    };
    let language = s.cfg.general.language;
    let look = s.look();
    let title = tr_ref(language, "Keyboard shortcuts");
    let field = TextInput::new(&sheet.query)
        .placeholder(tr_ref(language, "Search shortcuts"))
        .label(tr_ref(language, "Search shortcuts"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(None)
        .plain()
        .on_edit(|s: &mut UiState| match s.keys_sheet.as_mut() {
            Some(sheet) => &mut sheet.query,
            None => &mut s.closed_edit,
        })
        .on_key(|s: &mut UiState, ev: &rune::KeyEvent, tx: &mut rune::Tx| {
            if s.keys_sheet.is_none() {
                return super::keymap::dispatch(s, ev, tx);
            }
            super::keymap::intercept_global_key(s, ev, tx)
        })
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
                text(tr(language, "No shortcut matches"))
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
        .child(hint(k, "Esc", tr(language, "close")))
        .child(el().grow(1.0))
        .child(
            text(tr(language, "Settings → Shortcuts lists these too"))
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

fn which_key_item(k: &Kit, id: &str, keys_list: &[&str], label: &str) -> El {
    row()
        .key(format!("wk-{id}"))
        .w_full()
        .gap(8.0)
        .center_y()
        .py(2.0)
        .child(keycaps(k, keys_list))
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_base)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .truncate(),
        )
}

fn which_key_column(k: &Kit, id: &str, title: &str, items: Vec<El>) -> El {
    let mut column = col()
        .key(format!("wk-col-{id}"))
        .w_full()
        .min_w(0.0)
        .gap(4.0)
        .child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint)
                .pb(2.0),
        );
    for item in items {
        column = column.child(item);
    }
    column
}

fn mount_which_key(s: &UiState, k: &Kit, layer: &mut Vec<El>) {
    let language = s.cfg.general.language;
    let look = s.look();
    let leader_caps = keymap::leader_keycaps(KeyPlatform::current(), Some(&s.cfg.keymap));
    let group = s.leader_group;

    let badge_label = match group {
        Some(g) => format!("LEADER → {}", g.key().to_ascii_uppercase()),
        None => "LEADER".to_string(),
    };
    let hint = match group {
        Some(g) => format!(
            "{} · {}",
            tr_ref(language, g.label()),
            tr_ref(language, "Esc to close")
        ),
        None => tr_ref(
            language,
            "Group key or chord · Leader again to send to terminal · Esc to close",
        )
        .to_string(),
    };

    let badge = row()
        .key("which-key-badge")
        .h(22.0)
        .px(8.0)
        .center()
        .radius(k.radius.sm)
        .fill(k.c.bg_accent.alpha(0.18))
        .border(1.0, k.c.bg_accent.alpha(0.45))
        .child(
            text(badge_label)
                .font(k.ty.small_medium)
                .color(k.c.text_accent),
        );

    let head = row()
        .key("which-key-head")
        .w_full()
        .gap(10.0)
        .center_y()
        .px(16.0)
        .pt(12.0)
        .pb(10.0)
        .child(badge)
        .child(keycaps(k, &leader_caps))
        .child(
            text(hint)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .truncate(),
        );

    let grid = match group {
        None => {
            let groups = which_key_column(
                k,
                "groups",
                tr_ref(language, "Groups"),
                vec![
                    which_key_item(k, "g-s", &["s"], tr_ref(language, "Session")),
                    which_key_item(k, "g-w", &["w"], tr_ref(language, "Window")),
                    which_key_item(k, "g-t", &["t"], tr_ref(language, "Terminal")),
                    which_key_item(k, "g-g", &["g"], tr_ref(language, "Git")),
                    which_key_item(k, "g-p", &["p"], tr_ref(language, "Plugins")),
                    which_key_item(k, "g-o", &["o"], tr_ref(language, "Open")),
                ],
            );
            let direct = which_key_column(
                k,
                "direct",
                tr_ref(language, "Direct"),
                vec![
                    which_key_item(k, "focus", &["Space", ";"], tr_ref(language, "Focus")),
                    which_key_item(k, "palette", &["k"], tr_ref(language, "Palette")),
                    which_key_item(k, "new-session", &["n"], tr_ref(language, "New session")),
                    which_key_item(k, "sidebar", &["b"], tr_ref(language, "Sidebar")),
                    which_key_item(k, "inspector", &["i"], tr_ref(language, "Inspector")),
                    which_key_item(k, "pages", &["H/I/T/S"], tr_ref(language, "Pages")),
                    which_key_item(k, "help", &["?"], tr_ref(language, "Help")),
                ],
            );
            row()
                .key("which-key-grid")
                .w_full()
                .equal_columns(2)
                .gap(20.0)
                .px(16.0)
                .py(12.0)
                .child(groups)
                .child(direct)
        }
        Some(keymap::LeaderGroup::Session) => which_key_column(
            k,
            "session",
            tr_ref(language, "Session"),
            vec![
                which_key_item(k, "s-n", &["n"], tr_ref(language, "New session")),
                which_key_item(k, "s-p", &["p", "["], tr_ref(language, "Previous session")),
                which_key_item(k, "s-]", &["]"], tr_ref(language, "Next session")),
                which_key_item(k, "s-r", &["r"], tr_ref(language, "Recent session")),
                which_key_item(k, "s-c", &["c"], tr_ref(language, "Focus composer")),
                which_key_item(k, "s-f", &["/", "f"], tr_ref(language, "Search messages")),
            ],
        )
        .px(16.0)
        .py(12.0),
        Some(keymap::LeaderGroup::Window) => which_key_column(
            k,
            "window",
            tr_ref(language, "Window"),
            vec![
                which_key_item(k, "w-s", &["s"], tr_ref(language, "Split")),
                which_key_item(k, "w-h", &["h"], tr_ref(language, "Split stacked")),
                which_key_item(k, "w-v", &["v"], tr_ref(language, "Split side by side")),
                which_key_item(k, "w-j", &["j"], tr_ref(language, "Toggle split axis")),
                which_key_item(k, "w-z", &["z"], tr_ref(language, "Maximize")),
                which_key_item(k, "w-o", &["o"], tr_ref(language, "Pop out")),
                which_key_item(k, "w-e", &["e"], tr_ref(language, "Equalize")),
                which_key_item(k, "w-l", &["l"], tr_ref(language, "Cycle layout")),
                which_key_item(k, "w-x", &["x", "w"], tr_ref(language, "Close pane")),
            ],
        )
        .px(16.0)
        .py(12.0),
        Some(keymap::LeaderGroup::Terminal) => which_key_column(
            k,
            "terminal",
            tr_ref(language, "Terminal"),
            vec![
                which_key_item(k, "t-t", &["t"], tr_ref(language, "Toggle terminal")),
                which_key_item(k, "t-c", &["c"], tr_ref(language, "New Tab")),
                which_key_item(k, "t-x", &["x"], tr_ref(language, "Close tab")),
                which_key_item(k, "t-h", &["h", "v"], tr_ref(language, "Split")),
                which_key_item(k, "t-z", &["z"], tr_ref(language, "Zoom")),
                which_key_item(k, "t-f", &["/", "["], tr_ref(language, "Search")),
            ],
        )
        .px(16.0)
        .py(12.0),
        Some(keymap::LeaderGroup::Git) => which_key_column(
            k,
            "git",
            tr_ref(language, "Git"),
            vec![
                which_key_item(k, "git-g", &["g"], tr_ref(language, "Git inspector")),
                which_key_item(k, "git-d", &["d"], tr_ref(language, "Diff")),
            ],
        )
        .px(16.0)
        .py(12.0),
        Some(keymap::LeaderGroup::Plugin) => which_key_column(
            k,
            "plugins",
            tr_ref(language, "Plugins"),
            vec![
                which_key_item(k, "p-p", &["p"], tr_ref(language, "Sessions")),
                which_key_item(k, "p-k", &["k"], tr_ref(language, "Palette")),
            ],
        )
        .px(16.0)
        .py(12.0),
        Some(keymap::LeaderGroup::Open) => which_key_column(
            k,
            "open",
            tr_ref(language, "Open"),
            vec![
                which_key_item(k, "o-h", &["h"], tr_ref(language, "Home")),
                which_key_item(k, "o-i", &["i"], tr_ref(language, "Inbox")),
                which_key_item(k, "o-t", &["t"], tr_ref(language, "Tasks")),
                which_key_item(k, "o-s", &["s"], tr_ref(language, "Settings")),
                which_key_item(k, "o-?", &["?"], tr_ref(language, "Help")),
            ],
        )
        .px(16.0)
        .py(12.0),
    };

    let card = col()
        .key("which-key-card")
        .role("region")
        .label("Leader shortcuts")
        .w_full()
        .max_w(720.0)
        .child(head)
        .child(el().w_full().h(1.0).fill(k.c.border_muted))
        .child(grid);

    let mut card = look
        .panel(k, card, crate::theme::look::PANEL_RADIUS)
        .fill(k.c.surface_raised_base.alpha(0.97));
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        card = card.box_shadow(shadow);
    }

    layer.push(
        col()
            .key("which-key-overlay")
            .pin(Some(24.0), None, Some(24.0), Some(20.0))
            .items_center()
            .child(card),
    );
}
