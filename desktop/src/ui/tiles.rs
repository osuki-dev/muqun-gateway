//! Split view: two to four sessions at once, each pane its own rounded
//! card on the window (the main card steps aside), tiled as Hyprland's
//! dwindle layout does (`dwindle.rs`) with the window showing through the
//! gaps; dragging a gap moves that split. Each pane has a one-line header
//! and there is no toolbar over them. Focus is contrast, not decoration:
//! the other panes sit a step back (a scrim of the card colour, titles
//! muted) and focus cross-fades between them; a pane waiting on the owner
//! breathes a small warning dot by its title.
//!
//! Keys: `Primary+1..4` and `Primary+Alt+arrows` focus a pane,
//! `Primary+Alt+Shift+arrows` swap it with the one that way,
//! `Primary+\` adds one, `Primary+W` removes the focused one.

use rune::{col, el, row, text, Cursor, El, Key, KeyEvent, Phase, Tx};
use rune_kit::glyphs::{CLOSE, TRIANGLE_DOWN};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
};

use super::copy::{tr, tr_ref};
use super::dwindle;
use super::{
    activity_pill, ago, composer, now_ms, timeline, title_of, Activity, Mode, OpenMenu, UiState,
};
use crate::config::TileNode;
use crate::model::commands::Command;
use crate::theme::look::Look;

/// The see-through gap between two panes: the frame around the window's
/// panels.
const GAP: f32 = super::panes::FRAME;
/// A pane header's height.
const HEAD_H: f32 = 36.0;
/// How far an unfocused pane's content sits back: the alpha of the card
/// colour laid over it (light, dark), about a tenth of its contrast either
/// way (blending is in linear light, so dark needs more).
const UNFOCUSED_SCRIM: [f32; 2] = [0.12, 0.22];

/// `Primary+Alt+arrow` in split view: focus the pane that way; with Shift,
/// swap the focused pane's session with that pane's. Text fields pass it
/// here first (their own word moves take the plain chord).
pub fn arrow_key(s: &mut UiState, ev: &KeyEvent) -> bool {
    if !s.in_split() || !ev.mods.primary() || !ev.mods.alt {
        return false;
    }
    let toward = super::dwindle::neighbour(&s.tile_layout(), s.tile_grid, s.focus, &ev.key);
    match toward {
        // The terminal lends its Ctrl chords with Shift added (Windows and
        // Linux), so from there the arrows only ever move focus.
        Some(j) if ev.mods.shift && !s.term.sink.focused => {
            let i = s.focus;
            s.swap_tiles(i, j);
            true
        }
        Some(j) => {
            s.focus_tile(j);
            true
        }
        // An arrow toward the edge is still ours: it must not move the caret.
        None => matches!(ev.key, Key::Left | Key::Right | Key::Up | Key::Down),
    }
}
/// Fewest sessions to show in an empty tile's picker.
const PICK_ROWS: usize = 6;

/// Put session `asid` into empty tile `i`.
fn fill_tile(s: &mut UiState, i: usize, asid: &str) {
    if s.tiles.get(i).is_none_or(|t| !t.is_empty()) {
        return;
    }
    if let Some(open) = s.tile_of(asid) {
        s.focus_tile(open);
        return;
    }
    s.tiles[i].set_session(asid);
    s.tiles[i].slot = super::SessionSlot {
        draft: rune::TextEdit::new(s.cfg.general.drafts.get(asid).cloned().unwrap_or_default()),
        ..Default::default()
    };
    s.send(Command::OpenTile(muqun_gateway::agents::AgentSessionId(
        asid.to_string(),
    )));
    s.focus_tile(i);
    s.sync_watch();
    s.persist_tiles();
}

/// Put the interactive terminal into empty tile `i`.
/// When another tile already hosts a terminal, assigns an unused terminal tab
/// or opens a new terminal tab so both tiles display independent shells.
pub(crate) fn fill_terminal_tile(s: &mut UiState, i: usize) {
    if s.tiles.get(i).is_none_or(|t| !t.is_empty()) {
        return;
    }
    let used_tabs: Vec<usize> = s
        .tiles
        .iter()
        .enumerate()
        .filter(|(idx, t)| *idx != i && t.is_terminal())
        .map(|(_, t)| t.term_tab.unwrap_or(s.term.active_tab))
        .collect();
    s.close_terminal();
    if s.term.session.is_none() && s.term.tabs.is_empty() {
        s.start_terminal();
    }
    if !used_tabs.is_empty() {
        // Switching or opening a tab rebinds the focused tile; it keeps its own.
        let keep = (
            s.focus,
            s.tiles
                .get(s.focus)
                .and_then(|t| t.term_tab)
                .or(Some(s.term.active_tab)),
        );
        s.ensure_term_tabs_seeded(true);
        if let Some(free_idx) = (0..s.term.tabs.len()).find(|idx| !used_tabs.contains(idx)) {
            s.term_switch_tab(free_idx);
        } else {
            let cwd = s.terminal_cwd();
            s.term_new_tab_in(cwd, None);
        }
        if keep.0 != i {
            if let Some(t) = s.tiles.get_mut(keep.0).filter(|t| t.is_terminal()) {
                t.term_tab = keep.1;
            }
        }
    }
    s.tiles[i].set_terminal_tab(s.term.active_tab);
    s.focus_tile(i);
    s.persist_tiles();
}

/// Put a registered plugin into empty tile `i`.
pub fn fill_plugin_tile(s: &mut UiState, i: usize, plugin_id: &str) {
    if plugin_id == "terminal" {
        fill_terminal_tile(s, i);
        return;
    }
    if s.tiles.get(i).is_none_or(|t| !t.is_empty()) {
        return;
    }
    if let Some(plugin) = s.plugins.get(plugin_id) {
        let lang = s.cfg.general.language;
        let title = plugin.title(lang);
        let token = s.next_token;
        s.next_token += 1;
        s.tiles[i].content = super::TileContent::Custom {
            id: format!("{plugin_id}-{token}"),
            kind: plugin_id.to_string(),
            title,
        };
        s.focus_tile(i);
        s.persist_tiles();
    }
}

/// How to reach pane `i` from the keyboard, for its header's tooltip.
fn reach(language: crate::config::Language, i: usize) -> String {
    format!(
        "{} · {} {}",
        crate::platform::keys::label(&format!("Primary+{}", i + 1)),
        crate::platform::keys::label("Primary+Alt"),
        tr_ref(language, "+ arrows to move between panes"),
    )
}

/// A pane's header, one line in the toolbar's style: mark, title, agent,
/// status dot, and under the pointer its menu and close.
fn header(s: &UiState, k: &Kit, look: &Look, i: usize, place: Place, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let focused = i == s.focus;
    let content = &s.tiles[i].content;
    let mut head = row()
        .key("tile-head")
        .w_full()
        .h(HEAD_H)
        .pl(12.0 + place.clear)
        .motion(super::panes::SPRING)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .help(reach(language, i))
        .hover_group();
    // One weight either way, so focus moving never reflows a title; only
    // its ink changes, cross-fading with the pane's scrim.
    let title_font = k.ty.ui_medium;
    match content {
        super::TileContent::Session(asid) => {
            let info = s.session_by_id(asid);
            if let Some(info) = info {
                let activity = s.activity(info);
                head = head
                    .child(super::agents::mark(&info.agent_id, 14.0, s.dark).a11y_hidden())
                    .child(
                        text(title_of(info))
                            .key("tile-title")
                            .font(title_font)
                            .color(if focused {
                                k.c.text_base
                            } else {
                                k.c.text_muted
                            })
                            .motion(look.focus_fade())
                            .truncate()
                            .shrink()
                            // The agent's name gives way first in a narrow pane.
                            .min_w(48.0),
                    )
                    .child(
                        text(s.agent_name(&info.agent_id))
                            .key("tile-agent")
                            .font(k.ty.small)
                            .color(k.c.text_faint)
                            .truncate()
                            .shrink()
                            .min_w(0.0),
                    );
                if let Some(mark) = status_mark(k, look, activity, language) {
                    head = head.child(mark);
                }
            } else {
                head = head.child(text(asid.clone()).key("tile-title").font(title_font).color(
                    if focused {
                        k.c.text_base
                    } else {
                        k.c.text_muted
                    },
                ));
            }
        }
        super::TileContent::Terminal => {
            let title = s.term_tile_title(i);
            head = head
                .child(
                    rune::icon(&super::terminal_panel::TERMINAL_WINDOW, 14.0)
                        .color(if focused {
                            k.c.text_accent
                        } else {
                            k.c.icon_muted
                        })
                        .a11y_hidden(),
                )
                .child(
                    text(title)
                        .key("tile-title")
                        .font(title_font)
                        .color(if focused {
                            k.c.text_base
                        } else {
                            k.c.text_muted
                        })
                        .motion(look.focus_fade())
                        .truncate()
                        .shrink()
                        .min_w(48.0),
                )
                .child(
                    text(tr(language, "Terminal"))
                        .key("tile-agent")
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                );
        }
        super::TileContent::Diff(path) => {
            head = head
                .child(
                    rune::icon(&super::icons::BRANCH, 14.0)
                        .color(k.c.text_accent)
                        .a11y_hidden(),
                )
                .child(
                    text(format!("Diff: {path}"))
                        .key("tile-title")
                        .font(title_font)
                        .color(if focused {
                            k.c.text_base
                        } else {
                            k.c.text_muted
                        }),
                );
        }
        super::TileContent::Custom { title, kind, .. } => {
            let icon = s
                .plugins
                .get(kind)
                .map(|p| p.icon())
                .unwrap_or(&super::icons::GRID);
            head = head
                .child(rune::icon(icon, 14.0).color(k.c.text_accent).a11y_hidden())
                .child(
                    text(title.clone())
                        .key("tile-title")
                        .font(title_font)
                        .color(if focused {
                            k.c.text_base
                        } else {
                            k.c.text_muted
                        }),
                )
                .child(
                    text(kind.clone())
                        .key("tile-agent")
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                );
        }
        super::TileContent::Empty => {
            head = head.child(
                text(tr(language, "Pick a session"))
                    .key("tile-title")
                    .font(title_font)
                    .color(k.c.text_muted),
            );
        }
    }
    // The header row stands where a title bar would: its empty middle
    // moves the window.
    head = head.child(
        el().key("tile-spacer")
            .grow(1.0)
            .h_full()
            .window_drag()
            .a11y_hidden(),
    );
    // Move and close: visible under the pointer, always reachable by menu.
    let menu_open = s.menu == Some(OpenMenu::Tile(i));
    let mut menu = Menu::new(format!("tile-menu-{i}"))
        .trigger(
            IconButton::new(&TRIANGLE_DOWN)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Pane actions"))
                .expanded(menu_open)
                .on_press(move |s: &mut UiState| {
                    s.menu = (!menu_open).then_some(OpenMenu::Tile(i));
                })
                .build(k)
                .key(format!("tile-menu-btn-{i}")),
        )
        .open(menu_open)
        .on_close(|s: &mut UiState| s.menu = None);
    // Swap places with a neighbour, as the panes sit.
    let tree = s.tile_layout();
    let chords = super::keymap::labels(super::keymap::Action::SwapPaneToward);
    for (n, (key, en)) in [
        (Key::Left, "Swap with the pane on the left"),
        (Key::Right, "Swap with the pane on the right"),
        (Key::Up, "Swap with the pane above"),
        (Key::Down, "Swap with the pane below"),
    ]
    .into_iter()
    .enumerate()
    {
        if let Some(j) = super::dwindle::neighbour(&tree, s.tile_grid, i, &key) {
            menu = menu.item(
                MenuItem::new(tr(language, en))
                    .shortcut(if focused {
                        chords.get(n).cloned().unwrap_or_default()
                    } else {
                        String::new()
                    })
                    .on_select(move |s: &mut UiState| s.swap_tiles(i, j)),
            );
        }
    }
    match content {
        super::TileContent::Session(a) => {
            let a = a.clone();
            menu = menu.item(MenuItem::new(tr(language, "Show only this one")).on_select(
                move |s: &mut UiState| {
                    if let Some(at) = s.tile_of(&a) {
                        s.focus_tile(at);
                    }
                    let mut j = s.tiles.len();
                    while j > 0 {
                        j -= 1;
                        if s.tiles
                            .get(j)
                            .is_some_and(|t| t.asid.as_deref() != Some(a.as_str()))
                        {
                            s.close_tile(j);
                        }
                    }
                },
            ));
        }
        super::TileContent::Terminal => {
            menu = menu.item(MenuItem::new(tr(language, "Show only this one")).on_select(
                move |s: &mut UiState| {
                    if i < s.tiles.len() {
                        s.focus_tile(i);
                    }
                    let mut j = s.tiles.len();
                    while j > 0 {
                        j -= 1;
                        if j != s.focus && s.tiles.len() > 1 {
                            s.close_tile(j);
                        }
                    }
                },
            ));
        }
        super::TileContent::Custom { kind, .. } => {
            if let Some(plugin) = s.plugins.get(kind) {
                for cmd in plugin.commands() {
                    let cmd_id = cmd.id;
                    let p = plugin.clone();
                    menu = menu.item(
                        MenuItem::new(cmd.title)
                            .shortcut(cmd.shortcut.unwrap_or_default())
                            .on_select(move |s: &mut UiState| {
                                p.on_command(s, cmd_id);
                            }),
                    );
                }
            }
        }
        _ => {}
    }
    menu = menu
        .separator()
        .item(
            MenuItem::new(tr(language, "Remove from split view"))
                .shortcut(if focused {
                    super::keymap::label(super::keymap::Action::RemoveSplit)
                } else {
                    String::new()
                })
                .on_select(move |s: &mut UiState| s.close_tile(i)),
        )
        .item(
            MenuItem::new(tr(language, "Close split view"))
                .on_select(|s: &mut UiState| s.unsplit()),
        );
    let menu = menu.build(k, layer);
    head = head.child(if menu_open {
        menu
    } else {
        menu.group_opacity(0.0, 1.0)
    });
    head = head.child(
        IconButton::new(&CLOSE)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(tr_ref(language, "Remove from split view"))
            .shortcut(if focused {
                super::keymap::label(super::keymap::Action::RemoveSplit)
            } else {
                String::new()
            })
            .on_press(move |s: &mut UiState| s.close_tile(i))
            .build(k)
            .key(format!("tile-close-{i}"))
            .group_opacity(if focused { 0.7 } else { 0.0 }, 1.0),
    );
    // The split view has no toolbar: the top-right pane's header carries
    // what it held, adding a pane and the terminal, always in view.
    if place.corner {
        head = head.child(window_actions(s, k));
    }
    head
}

/// Add to split view and the terminal toggle, at the trailing end of the
/// top-right pane's header (the window's corner, where the toolbar had them).
fn window_actions(s: &UiState, k: &Kit) -> El {
    use super::keymap::{tip, Action};
    let language = s.cfg.general.language;
    let mut out = row()
        .key("split-actions")
        .gap(2.0)
        .pl(6.0)
        .center_y()
        .child(
            el().key("split-actions-rule")
                .w(1.0)
                .h(14.0)
                .mr(4.0)
                .fill(k.c.border_muted)
                .a11y_hidden(),
        );
    if s.tiles.len() < super::MAX_TILES {
        out = out.child(
            IconButton::new(&super::icons::PLUS)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Add to split view"))
                .help(tip(tr_ref(language, "Add to split view"), Action::AddSplit))
                .on_press(|s: &mut UiState| s.split())
                .build(k)
                .key("split-add"),
        );
    }
    let has_term_tile = s.has_terminal_tile();
    out.child(
        IconButton::new(&super::terminal_panel::TERMINAL_WINDOW)
            .variant(if s.term.open || has_term_tile {
                IconButtonVariant::Contrast
            } else {
                IconButtonVariant::GhostMuted
            })
            .control_size(ControlSize::Small)
            .label(tr_ref(language, "Terminal"))
            .help(tip(tr_ref(language, "Terminal"), Action::ToggleTerminal))
            .on_press_tx(|s: &mut UiState, tx: &mut Tx| {
                let mods = tx.mods();
                if mods.ctrl || mods.cmd || mods.alt {
                    s.open_terminal_tile();
                } else if s.has_terminal_tile() {
                    if let Some(i) = s.tile_of_terminal() {
                        s.focus_tile(i);
                    }
                } else if s.term.open {
                    s.close_terminal();
                } else {
                    s.open_terminal();
                }
            })
            .build(k)
            .key("terminal-toggle"),
    )
}

/// What a pane's header says about its session, as a bare mark by the
/// title: the spinner while it works, a warning dot breathing slowly while
/// it waits on the owner (still under reduce motion), a danger dot when it
/// failed. Nothing when idle.
fn status_mark(
    k: &Kit,
    look: &Look,
    activity: Activity,
    language: crate::config::Language,
) -> Option<El> {
    let dot = |c: rune::Color| el().size(7.0, 7.0).radius(3.5).fill(c);
    let (mark, word) = match activity {
        Activity::Idle => return None,
        Activity::Working => (
            rune_kit::Spinner::new()
                .size(10.0)
                .color(k.c.text_accent)
                .build(k),
            tr(language, "Working"),
        ),
        Activity::NeedsYou => (
            look.breathe(dot(k.c.warning.icon)),
            tr(language, "Needs you"),
        ),
        Activity::Failed => (dot(k.c.danger.icon), tr(language, "Failed")),
    };
    Some(
        row()
            .key("tile-status")
            .size(14.0, 14.0)
            .center()
            .role("status")
            .label(word.clone())
            .help(word)
            .child(mark.a11y_hidden()),
    )
}

/// A smart folded composer for an unfocused tile: shows identity mark,
/// dynamic agent placeholder or draft, status/queue badges, and brings
/// the tile into focus when pressed.
fn parked_composer(s: &UiState, k: &Kit, look: &Look, i: usize) -> El {
    let language = s.cfg.general.language;
    let asid = s.tiles[i].asid.as_deref().unwrap_or_default();
    let info = s.session_by_id(asid);
    let agent_id = info.map(|x| x.agent_id.as_str()).unwrap_or("claude");
    let agent_name = info
        .map(|x| s.agent_name(&x.agent_id))
        .unwrap_or_else(|| "Agent".to_string());
    let activity = info.map(|x| s.activity(x)).unwrap_or(Activity::Idle);

    let draft = s.tiles[i].slot.draft.text.trim().to_string();
    let is_draft = !draft.is_empty();
    let (line, ink) = if is_draft {
        (draft, k.c.text_base)
    } else {
        (
            tr(language, &format!("Message {agent_name}…")),
            k.c.text_faint,
        )
    };
    let queued = s.tiles[i].slot.queue.len();

    let brand_c = super::agents::color(agent_id, s.dark);
    let mark_box = el()
        .key("parked-agent-mark")
        .size(22.0, 22.0)
        .radius(6.0)
        .fill(brand_c.alpha(if s.dark { 0.16 } else { 0.10 }))
        .border(0.5, brand_c.alpha(if s.dark { 0.32 } else { 0.22 }))
        .center()
        .child(super::agents::mark(agent_id, 12.0, s.dark).a11y_hidden());

    let mut strip = row()
        .key("tile-composer")
        .w_full()
        .h(42.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .role("button")
        .label(tr_ref(language, "Focus this pane"))
        .cursor(Cursor::Text)
        .on_press(move |s: &mut UiState| s.focus_tile(i))
        .child(mark_box)
        .child(
            text(line)
                .font(k.ty.ui)
                .color(ink)
                .truncate()
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        );

    if is_draft {
        strip = strip.child(
            rune_kit::Badge::new(tr_ref(language, "Draft"))
                .variant(rune_kit::BadgeVariant::Subtle)
                .build(k),
        );
    }
    if queued > 0 {
        strip = strip.child(
            rune_kit::Badge::new(format!("{queued} {}", tr_ref(language, "queued")))
                .variant(rune_kit::BadgeVariant::Subtle)
                .build(k),
        );
    }
    if activity != Activity::Idle {
        strip = strip.child(activity_pill(k, look, activity, language).key("parked-activity"));
    }

    let column = super::timeline::FEED_MAX_W;
    col()
        .key("tile-composer-pad")
        .w_full()
        .items_center()
        .px(16.0)
        .pb(14.0)
        .child(look.pressable(k, look.row_card(k, strip.w_full().max_w(column), 10.0)))
}

/// An empty tile: pick a recent session or start a new one into it.
fn picker(s: &UiState, k: &Kit, look: &Look, i: usize) -> El {
    let language = s.cfg.general.language;
    let now = now_ms();
    let open: Vec<&str> = s.tiles.iter().filter_map(|t| t.asid.as_deref()).collect();
    let mut list = col().key("tile-pick").w_full().max_w(420.0).gap(6.0);
    for info in s
        .sessions
        .iter()
        .filter(|x| !open.contains(&x.asid.0.as_str()))
        .take(PICK_ROWS)
    {
        let asid = info.asid.0.clone();
        let activity = s.activity(info);
        let mut line = row()
            .key(format!("pick-{}", info.asid.0))
            .w_full()
            .h(44.0)
            .px(12.0)
            .gap(10.0)
            .center_y()
            .role("button")
            .label(title_of(info))
            .on_press(move |s: &mut UiState| fill_tile(s, i, &asid))
            .child(super::agents::mark(&info.agent_id, 16.0, s.dark).a11y_hidden())
            .child(
                col()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0)
                    .gap(1.0)
                    .child(
                        text(title_of(info))
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base)
                            .truncate(),
                    )
                    .child(
                        text(format!(
                            "{} · {}",
                            s.agent_name(&info.agent_id),
                            ago(info.updated_ms, now, language)
                        ))
                        .font(k.ty.caption())
                        .color(k.c.text_faint)
                        .truncate(),
                    ),
            );
        if activity != Activity::Idle {
            line = line.child(activity_pill(k, look, activity, language));
        }
        list = list.child(look.pressable(k, look.row_card(k, line, 10.0)));
    }
    let mut actions = row().gap(8.0).wrap().center_y();
    actions = actions.child(
        Button::new(tr_ref(language, "New session…"))
            .variant(ButtonVariant::Outline)
            .control_size(ControlSize::Regular)
            .on_press(move |s: &mut UiState| {
                s.pending_tile = Some(i);
                s.go(Mode::Home);
            })
            .build(k)
            .key(format!("tile-new-{i}")),
    );
    for plugin in s.plugins.tile_plugins() {
        let plugin_id = plugin.id();
        let plugin_title = plugin.title(language);
        let plugin_icon = plugin.icon();
        let pid = plugin_id.to_string();
        actions = actions.child(
            Button::new(plugin_title)
                .icon(plugin_icon)
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press(move |s: &mut UiState| {
                    fill_plugin_tile(s, i, &pid);
                })
                .build(k)
                .key(format!("tile-plugin-{plugin_id}-{i}")),
        );
    }
    col()
        .key("tile-empty")
        .grow(1.0)
        .w_full()
        .min_h(0.0)
        .center()
        .gap(16.0)
        .px(20.0)
        .child(
            text(tr(language, "Open a session in this pane"))
                .font(k.ty.body_medium)
                .color(k.c.text_base),
        )
        .child(list)
        .child(actions)
}

/// Where pane `i` sits among the others, for what its header carries.
#[derive(Clone, Copy, Default)]
struct Place {
    /// Points its header keeps clear at the leading edge (the window's
    /// corner controls, for the top-left pane).
    clear: f32,
    /// It is the top-right pane: its header holds the window's actions.
    corner: bool,
}

/// Pane `i` as its own card: a rounded floor like the sidebar's, a header
/// line, its timeline and composer.
fn tile(
    s: &UiState,
    k: &Kit,
    look: &Look,
    i: usize,
    place: Place,
    scale: f32,
    layer: &mut Vec<El>,
) -> El {
    let focused = i == s.focus;
    let t = &s.tiles[i];
    let id = match &t.content {
        super::TileContent::Session(asid) => format!("tile-{asid}"),
        super::TileContent::Terminal => {
            if s.tiles.iter().take(i).any(|x| x.is_terminal()) {
                format!("tile-terminal-{i}")
            } else {
                "tile-terminal".to_string()
            }
        }
        super::TileContent::Diff(p) => format!("tile-diff-{p}"),
        super::TileContent::Custom { id, .. } => format!("tile-custom-{id}"),
        super::TileContent::Empty => format!("tile-empty-{i}"),
    };
    let radius = k.radius.lg;
    let (under, over) = super::shell::card_floor_colors(k, look);
    let mut body = col()
        .key(id.clone())
        .matched(id.clone())
        .grow(1.0)
        // Panes share a cramped window by the split, rather than the first
        // giving all its room up to the others' floors.
        .min_w(0.0)
        .min_h(0.0)
        .radius(radius)
        .clip_rounded(radius)
        .fill(under)
        .border(1.0, k.c.border_muted)
        .motion(crate::theme::look::MOVE)
        .on_press(move |s: &mut UiState| s.focus_tile(i));
    if let Some(over) = over {
        body = body.child(
            el().key("tile-floor")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(over)
                .a11y_hidden(),
        );
    }
    body = body.child(header(s, k, look, i, place, layer)).child(
        el().key("tile-rule")
            .w_full()
            .h(1.0)
            .fill(k.c.border_muted)
            .a11y_hidden(),
    );
    match (&t.content, focused) {
        (super::TileContent::Empty, _) => body = body.child(picker(s, k, look, i)),
        (super::TileContent::Terminal, _) => {
            let scheme = if k.dark {
                crate::theme::color_scheme::ColorScheme::Dark
            } else {
                crate::theme::color_scheme::ColorScheme::Light
            };
            body = body.child(super::terminal_panel::tile_body_for(
                s,
                k,
                scheme,
                scale,
                layer,
                Some(i),
            ));
        }
        (super::TileContent::Custom { id, kind, .. }, _) => {
            let ctx = crate::plugin::PluginViewContext {
                s,
                k,
                look,
                tile_id: id,
                scale,
            };
            if let Some(plugin) = s.plugins.get(kind) {
                if let Some(view) = plugin.render_tile(&ctx, layer) {
                    body = body.child(view);
                }
            }
        }
        (super::TileContent::Diff(_), _) => {
            let ctx = crate::plugin::PluginViewContext {
                s,
                k,
                look,
                tile_id: "diff",
                scale,
            };
            if let Some(plugin) = s.plugins.get("diff") {
                if let Some(view) = plugin.render_tile(&ctx, layer) {
                    body = body.child(view);
                }
            }
        }
        (super::TileContent::Session(_), true) => {
            let feed = if s.show_diff {
                timeline::changes(s, k)
            } else {
                timeline::view(s, k)
            };
            let feed = feed
                .on_drop_files(|s: &mut UiState, files, _| composer::attach_dropped_files(s, files))
                .on_drop_hover(|s: &mut UiState, over, _| s.drop_hover = over);
            body = body.child(feed).child(composer::view(s, k, layer));
        }
        (super::TileContent::Session(asid), false) => {
            body = body
                .child(timeline::session_view(s, k, asid))
                .child(parked_composer(s, k, look, i));
        }
    }
    // Focus is contrast: the other panes' content sits a step back under a
    // scrim of the card's own colour (one quad, no layer, no blur) that
    // cross-fades as focus moves. The header keeps clear of it; its title
    // mutes instead.
    body = body.child(
        el().key("tile-scrim")
            .pin(Some(0.0), Some(HEAD_H + 1.0), Some(0.0), Some(0.0))
            .fill(k.c.bg_base.alpha(UNFOCUSED_SCRIM[usize::from(look.dark)]))
            .opacity(if focused { 0.0 } else { 1.0 })
            .opacity_motion(look.focus_fade())
            .a11y_hidden(),
    );
    body.role("region").label(format!(
        "{} {}",
        tr_ref(s.cfg.general.language, "Pane"),
        i + 1
    ))
}

/// The gap between the two halves of the split at `path`: the window shows
/// through it, and dragging it moves the split. `room` is the length the
/// halves share along the split.
fn gap(s: &UiState, k: &Kit, path: &[bool], across: bool, ratio: f32, room: f32) -> El {
    let key = path_key("tile-gap", path);
    let path_clone = path.to_vec();
    let is_dragging = s.tile_drag.as_ref() == Some(&path_clone);

    let line_color = if is_dragging {
        k.c.bg_accent
    } else {
        k.c.border_muted.alpha(0.6)
    };
    let grip_color = if is_dragging {
        k.c.bg_accent
    } else {
        k.c.text_muted.alpha(0.7)
    };

    let path_drag = path_clone.clone();
    let mut handle = el()
        .key(key)
        .cursor(if across {
            Cursor::ResizeColumn
        } else {
            Cursor::ResizeRow
        })
        .role("separator")
        .label(tr_ref(s.cfg.general.language, "Resize split view"))
        .a11y_value(format!("{:.0}%", ratio * 100.0))
        .on_drag(
            move |s: &mut UiState, d: &mut rune::Drag, _: &mut Tx| match d.phase {
                Phase::Down => {
                    d.data = ratio;
                    s.tile_drag = Some(path_drag.clone());
                }
                Phase::Move if room > 0.0 => {
                    let moved = if across { d.dx } else { d.dy };
                    s.set_tile_ratio(&path_drag, d.data + moved / room);
                }
                Phase::Up => {
                    s.tile_drag = None;
                    s.persist_tiles();
                }
                _ => {}
            },
        );

    if across {
        let center_offset = (room * 0.5 - 16.0).max(0.0);
        handle = handle
            .w(GAP)
            .h_full()
            .center()
            .child(el().w(1.0).h_full().fill(line_color))
            .child(
                el().absolute(2.0, center_offset)
                    .w(4.0)
                    .h(32.0)
                    .radius(2.0)
                    .fill(grip_color)
                    .hover(k.c.bg_accent),
            );
    } else {
        let center_offset = (room * 0.5 - 16.0).max(0.0);
        handle = handle
            .h(GAP)
            .w_full()
            .center()
            .child(el().h(1.0).w_full().fill(line_color))
            .child(
                el().absolute(center_offset, 2.0)
                    .w(32.0)
                    .h(4.0)
                    .radius(2.0)
                    .fill(grip_color)
                    .hover(k.c.bg_accent),
            );
    }
    handle
}

fn path_key(prefix: &str, path: &[bool]) -> String {
    let bits: String = path.iter().map(|&b| if b { '1' } else { '0' }).collect();
    format!("{prefix}-{bits}")
}

/// The tree at `path` as nested rows and columns of cards and gaps.
fn arrange(
    s: &UiState,
    k: &Kit,
    node: &TileNode,
    path: &mut Vec<bool>,
    rect: dwindle::Rect,
    cards: &mut Vec<Option<El>>,
) -> El {
    match node {
        TileNode::Leaf(i) => cards
            .get_mut(*i)
            .and_then(Option::take)
            .unwrap_or_else(|| el().key(format!("tile-missing-{i}"))),
        TileNode::Split {
            across,
            ratio,
            a,
            b,
        } => {
            let (ra, rb) = dwindle::cut(rect, *across, *ratio);
            let room = if *across { rect[2] } else { rect[3] } - GAP;
            let gap = gap(s, k, path, *across, *ratio, room);
            path.push(false);
            let first = arrange(s, k, a, path, ra, cards);
            path.pop();
            path.push(true);
            let second = arrange(s, k, b, path, rb, cards);
            path.pop();
            let line = if *across { row() } else { col() };
            line.key(path_key("tile-split", path))
                .grow(1.0)
                .min_w(0.0)
                .min_h(0.0)
                .child(first.grow(*ratio).min_w(0.0).min_h(0.0))
                .child(gap)
                .child(second.grow(1.0 - *ratio).min_w(0.0).min_h(0.0))
        }
    }
}

/// How far the top-left pane's header keeps clear of the window's corner
/// (the traffic lights and the sidebar's toggle, which float there while
/// the sidebar is closed), `inset` the lights' width. The kit's toolbar
/// would clear them by its padding; the panes have none. Mirrors
/// `SplitView`: the lights (or 12 points of edge), 8 points, the Large
/// toggle's 28, 8 points, less the frame the panes start at and the
/// header's own 12.
pub fn toggle_clearance(s: &UiState, inset: f32) -> f32 {
    if s.sidebar.is_open() {
        return 0.0;
    }
    let lights = if inset > 0.0 { inset + 8.0 } else { 12.0 };
    lights + 28.0 + 8.0 - super::panes::FRAME - 12.0
}

/// The panes, each its own card on the window with gaps between (the
/// main card steps aside for them); `clear` keeps the top-left header
/// clear of the window's corner ([`toggle_clearance`]).
pub fn view(s: &UiState, k: &Kit, clear: f32, scale: f32, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let tree = s.tile_layout();
    let [w, h] = s.tile_grid;
    let size = [w.max(1.0), h.max(1.0)];
    let placed = dwindle::rects(&tree, [0.0, 0.0, size[0], size[1]]);
    let top_left = placed
        .iter()
        .min_by(|x, y| (x.1[1] + x.1[0]).total_cmp(&(y.1[1] + y.1[0])))
        .map(|p| p.0);
    let top_right = placed
        .iter()
        .filter(|(_, r)| r[1] < 1.0)
        .max_by(|x, y| x.1[0].total_cmp(&y.1[0]))
        .map(|p| p.0);
    let mut cards: Vec<Option<El>> = (0..s.tiles.len())
        .map(|i| {
            let place = Place {
                clear: if Some(i) == top_left { clear } else { 0.0 },
                corner: Some(i) == top_right,
            };
            Some(tile(s, k, &look, i, place, scale, layer))
        })
        .collect();
    let body = arrange(
        s,
        k,
        &tree,
        &mut Vec::new(),
        [0.0, 0.0, size[0], size[1]],
        &mut cards,
    );
    // No toolbar: the panes rise under the window's empty toolbar row, so
    // their headers sit at the top, a frame's width from the window's edge.
    el().key("tiles-frame").w_full().grow(1.0).min_h(0.0).child(
        col()
            .key("tiles")
            .pin(
                Some(0.0),
                // An error card above them keeps the row's place.
                Some(if s.error.is_some() {
                    0.0
                } else {
                    -rune_kit::shell::TOOLBAR_HEIGHT
                }),
                Some(0.0),
                Some(0.0),
            )
            .on_layout(|s: &mut UiState, rect: [f32; 4]| s.tile_grid = [rect[2], rect[3]])
            .child(body),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Tile;
    use muqun_gateway::agents::AgentSessionInfo;
    use rune::TextEdit;

    #[test]
    fn parked_composer_shows_agent_identity_and_dynamic_placeholder() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let session: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s1",
            "agent_id": "claude",
            "title": "Fix bug",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.sessions = vec![session];
        s.tiles = vec![Tile::session("s1")];

        let k = Kit::neutral(s.dark);
        let look = s.look();
        let _el = parked_composer(&s, &k, &look, 0);

        // Test in Chinese
        s.cfg.general.language = crate::config::Language::ZhCn;
        let _el_zh = parked_composer(&s, &k, &look, 0);

        // Test with draft
        s.tiles[0].slot.draft = TextEdit::new("Draft message to Claude");
        let _el_draft = parked_composer(&s, &k, &look, 0);

        // Test with busy status
        s.sessions[0].status = muqun_gateway::agents::AgentSessionStatus::Busy;
        let _el_busy = parked_composer(&s, &k, &look, 0);
    }

    #[test]
    fn terminal_tile_lifecycle_and_split_operations() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("s1");
        assert_eq!(s.selected.as_deref(), Some("s1"));
        assert!(!s.in_split());

        // Open terminal beside the current session
        s.open_terminal_tile();
        assert!(s.in_split());
        assert_eq!(s.tiles.len(), 2);
        assert!(s.tiles[0].is_session());
        assert_eq!(s.tiles[0].asid.as_deref(), Some("s1"));
        assert!(s.tiles[1].is_terminal());
        assert!(s.has_terminal_tile());
        assert_eq!(s.focus, 1);
        assert!(s.term.sink.focused);

        // Focus session tile
        s.focus_tile(0);
        assert_eq!(s.focus, 0);
        assert!(s.draft.focused);
        assert!(!s.term.sink.focused);

        // Focus back to terminal tile
        s.focus_tile(1);
        assert_eq!(s.focus, 1);
        assert!(!s.draft.focused);
        assert!(s.term.sink.focused);

        // Swap tiles
        s.swap_tiles(0, 1);
        assert!(s.tiles[0].is_terminal());
        assert!(s.tiles[1].is_session());
        assert_eq!(s.focus, 0);

        // Persist and restore
        s.persist_tiles();
        assert_eq!(s.cfg.ui.tiles, vec!["terminal", "s1"]);

        // Close terminal tile
        s.close_tile(0);
        assert_eq!(s.tiles.len(), 0);
        assert_eq!(s.selected.as_deref(), Some("s1"));
        assert!(!s.in_split());
        assert!(!s.has_terminal_tile());
    }

    #[test]
    fn custom_plugin_tile_lifecycle_and_rendering() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("s1");
        s.split();
        assert_eq!(s.tiles.len(), 2);
        assert!(s.tiles[1].is_empty());

        // Fill empty tile with sysinfo plugin
        fill_plugin_tile(&mut s, 1, "sysinfo");
        assert!(matches!(
            &s.tiles[1].content,
            crate::ui::TileContent::Custom { kind, .. } if kind == "sysinfo"
        ));

        let k = Kit::neutral(s.dark);
        let look = s.look();
        let place = Place::default();
        let mut layer = Vec::new();

        // Render sysinfo tile
        let _el = tile(&s, &k, &look, 1, place, 1.0, &mut layer);

        // Fill with quicknotes plugin
        s.tiles[1].content = crate::ui::TileContent::Empty;
        fill_plugin_tile(&mut s, 1, "quicknotes");
        assert!(matches!(
            &s.tiles[1].content,
            crate::ui::TileContent::Custom { kind, .. } if kind == "quicknotes"
        ));

        let _el_notes = tile(&s, &k, &look, 1, place, 1.0, &mut layer);
    }

    #[test]
    fn multiple_independent_terminal_tiles_side_by_side() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("s1");
        s.open_terminal_tile();
        assert_eq!(s.focus, 1);
        // Split again from the focused terminal: a second, separate shell.
        s.open_terminal_tile();
        let terms: Vec<usize> = (0..s.tiles.len())
            .filter(|i| s.tiles[*i].is_terminal())
            .collect();
        assert_eq!(terms.len(), 2);
        let (a, b) = (terms[0], terms[1]);
        assert_eq!(s.focus, b);
        assert_eq!(s.term.tabs.len(), 2);
        let tab_a = s.tiles[a].term_tab.unwrap();
        let tab_b = s.tiles[b].term_tab.unwrap();
        assert_ne!(tab_a, tab_b);
        assert_eq!(s.term.active_tab, tab_b);
        assert_eq!(s.tile_of_terminal(), Some(b));
        assert_eq!(s.tile_terminal_tab(a), Some(tab_a));

        // Focus follows: the active shell is the focused tile's own.
        s.focus_tile(a);
        assert_eq!(s.term.active_tab, tab_a);
        assert_eq!(s.tile_of_terminal(), Some(a));
        assert_eq!(s.tiles[b].term_tab, Some(tab_b));

        // Switching tile a to b's tab trades the two tiles' shells.
        s.term_switch_tab_for_tile(Some(a), tab_b);
        assert_eq!(s.tiles[a].term_tab, Some(tab_b));
        assert_eq!(s.tiles[b].term_tab, Some(tab_a));
        assert_eq!(s.term.active_tab, tab_b);

        // Both tiles render without colliding (only the live one has the sink).
        let k = Kit::neutral(s.dark);
        let look = s.look();
        let mut layer = Vec::new();
        let _ea = tile(&s, &k, &look, a, Place::default(), 1.0, &mut layer);
        let _eb = tile(&s, &k, &look, b, Place::default(), 1.0, &mut layer);
        assert_ne!(s.term_tile_title(a), String::new());

        // Closing the only spare tab a tile shows closes that tile.
        let shown_by_b = s.tiles[b].term_tab.unwrap();
        s.term_close_tab(shown_by_b);
        assert_eq!(s.tiles.iter().filter(|t| t.is_terminal()).count(), 1);
        assert_eq!(s.term.tabs.len(), 1);
        let only = s.tile_of_terminal().unwrap();
        assert_eq!(s.tiles[only].term_tab, Some(0));
        assert_eq!(s.term.active_tab, 0);
    }

    #[test]
    fn split_resizer_gap_and_tile_drag() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("s1");
        s.open_terminal_tile();
        assert!(s.in_split());

        let path = vec![];
        s.set_tile_ratio(&path, 0.4);
        let tree = s.tile_layout();
        match tree {
            crate::config::TileNode::Split { ratio, .. } => {
                assert!((ratio - 0.4).abs() < 0.001);
            }
            _ => panic!("expected split node"),
        }

        let k = Kit::neutral(s.dark);
        let g = gap(&s, &k, &path, true, 0.4, 800.0);
        let _ = g;

        // Test vertical split gap as well
        let g_vert = gap(&s, &k, &path, false, 0.4, 600.0);
        let _ = g_vert;
    }
}
