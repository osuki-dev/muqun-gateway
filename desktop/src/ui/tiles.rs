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

/// Active header drag while reordering, edge-splitting, or moving a floating tile.
#[derive(Debug, Clone)]
pub struct TileCardDrag {
    pub src: usize,
    /// Pointer in the tiles stage, when known.
    pub pointer: [f32; 2],
    pub over: Option<usize>,
    pub zone: Option<dwindle::DropZone>,
    /// Floating/popped card geometry at drag start, if moving that card.
    pub start_rect: Option<[f32; 4]>,
}

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
    let toward = neighbour(s, &ev.key);
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
/// One geometry source for rendering, directional focus and drag targets.
fn scrolling_rects(s: &UiState) -> Vec<(usize, dwindle::Rect)> {
    let visible: Vec<_> = (0..s.tiles.len()).filter(|&i| !s.tiles[i].popped).collect();
    let w = s.tile_grid[0].max(1.0);
    let h = s.tile_grid[1].max(1.0);
    let width = if visible.len() <= 1 {
        w
    } else {
        (w * 0.62).max(360.0).min(w)
    };
    let focus = visible.iter().position(|&i| i == s.focus).unwrap_or(0) as f32;
    let total = visible.len() as f32 * (width + GAP) - GAP;
    let offset = (focus * (width + GAP) - (w - width) * 0.5).clamp(0.0, (total - w).max(0.0));
    visible
        .into_iter()
        .enumerate()
        .map(|(n, i)| (i, [n as f32 * (width + GAP) - offset, 0.0, width, h]))
        .collect()
}

fn visible(node: &TileNode, s: &UiState) -> bool {
    match node {
        TileNode::Leaf(i) => s.tiles.get(*i).is_some_and(|tile| !tile.popped),
        TileNode::Split { a, b, .. } => visible(a, s) || visible(b, s),
    }
}

fn background_rects(
    node: &TileNode,
    s: &UiState,
    rect: dwindle::Rect,
    out: &mut Vec<(usize, dwindle::Rect)>,
) {
    match node {
        TileNode::Leaf(i) => {
            if visible(node, s) {
                out.push((*i, rect));
            }
        }
        TileNode::Split {
            across,
            ratio,
            a,
            b,
        } => match (visible(a, s), visible(b, s)) {
            (true, false) => background_rects(a, s, rect, out),
            (false, true) => background_rects(b, s, rect, out),
            (true, true) => {
                let (ra, rb) = dwindle::cut(rect, *across, *ratio);
                background_rects(a, s, ra, out);
                background_rects(b, s, rb, out);
            }
            _ => {}
        },
    }
}

fn placed_rects(s: &UiState) -> Vec<(usize, dwindle::Rect)> {
    let mut rects = match s.tile_layout_mode() {
        crate::config::TileLayoutMode::Scrolling => scrolling_rects(s),
        crate::config::TileLayoutMode::Floating => Vec::new(),
        crate::config::TileLayoutMode::Dwindle => {
            let mut out = Vec::new();
            background_rects(
                &s.tile_layout(),
                s,
                [0.0, 0.0, s.tile_grid[0].max(1.0), s.tile_grid[1].max(1.0)],
                &mut out,
            );
            out
        }
    };
    for (i, t) in s.tiles.iter().enumerate() {
        if t.popped || s.tile_layout_mode() == crate::config::TileLayoutMode::Floating {
            rects.push((
                i,
                fit_floating(
                    t.floating_rect
                        .unwrap_or_else(|| UiState::default_floating_rect(i, s.tile_grid)),
                    s.tile_grid,
                ),
            ));
        }
    }
    rects
}

/// Keep every floating titlebar and resize handle reachable after a window resize.
fn fit_floating(r: dwindle::Rect, stage: [f32; 2]) -> dwindle::Rect {
    let sw = stage[0].max(1.0);
    let sh = stage[1].max(1.0);
    let w = r[2].max(280.0).min(sw);
    let h = r[3].max(180.0).min(sh);
    [r[0].clamp(0.0, sw - w), r[1].clamp(0.0, sh - h), w, h]
}

pub(super) fn neighbour(s: &UiState, key: &Key) -> Option<usize> {
    if s.tile_layout_mode() == crate::config::TileLayoutMode::Dwindle
        && !s.tiles.iter().any(|t| t.popped)
    {
        return dwindle::neighbour(&s.tile_layout(), s.tile_grid, s.focus, key);
    }
    let rects = placed_rects(s);
    let (_, from) = rects.iter().find(|(i, _)| *i == s.focus)?;
    let center = |r: &dwindle::Rect| [r[0] + r[2] * 0.5, r[1] + r[3] * 0.5];
    let a = center(from);
    rects
        .iter()
        .filter(|(i, _)| *i != s.focus)
        .filter_map(|(i, r)| {
            let b = center(r);
            let (forward, side) = match key {
                Key::Left => (a[0] - b[0], (a[1] - b[1]).abs()),
                Key::Right => (b[0] - a[0], (a[1] - b[1]).abs()),
                Key::Up => (a[1] - b[1], (a[0] - b[0]).abs()),
                Key::Down => (b[1] - a[1], (a[0] - b[0]).abs()),
                _ => return None,
            };
            (forward > 0.5).then_some((*i, forward + side * 2.0))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

fn layout_motion(s: &UiState) -> rune::Spring {
    if s.cfg.appearance.reduced_motion || s.tile_card_drag.is_some() || s.tile_drag.is_some() {
        rune::Spring::NONE
    } else {
        rune::Spring::new(0.38, 0.06)
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
    if let Some(plugin) = s.plugins.get_enabled(plugin_id, &s.cfg.plugins_disabled) {
        let lang = s.cfg.general.language;
        let title = plugin.title(lang);
        let token = s.next_token;
        s.next_token += 1;
        s.tiles[i].content = super::TileContent::Custom {
            id: format!("{plugin_id}-{token}"),
            kind: plugin_id.to_string(),
            title,
        };
        if plugin_id == "sysinfo" {
            s.refresh_runtimes();
        }
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
    // The header's empty middle: drag to reorder/split panes, or move a
    // floating/popped card. Outside split view it still moves the window.
    head = head.child(if s.in_split() {
        el().key("tile-spacer")
            .grow(1.0)
            .h_full()
            .cursor(Cursor::Grab)
            .on_drag(move |s: &mut UiState, d: &mut rune::Drag, _: &mut Tx| {
                header_tile_drag(s, i, d);
            })
            .a11y_hidden()
    } else {
        el().key("tile-spacer")
            .grow(1.0)
            .h_full()
            .window_drag()
            .a11y_hidden()
    });
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
            let kind_page = kind.clone();
            let kind_drawer = kind.clone();
            menu = menu
                .item(
                    MenuItem::new(tr(language, "Open as Full-Screen Page")).on_select(
                        move |s: &mut UiState| {
                            s.switch_plugin_surface(&kind_page, crate::plugin::PluginSlot::Page);
                        },
                    ),
                )
                .item(
                    MenuItem::new(tr(language, "Move to Right Drawer")).on_select(
                        move |s: &mut UiState| {
                            s.switch_plugin_surface(
                                &kind_drawer,
                                crate::plugin::PluginSlot::Drawer,
                            );
                        },
                    ),
                );
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
    let zoomed = s.zoomed_tile == Some(i);
    let popped = s.tiles.get(i).is_some_and(|t| t.popped);
    menu = menu
        .separator()
        .item(
            MenuItem::new(tr(
                language,
                if zoomed {
                    "Restore pane size"
                } else {
                    "Maximize pane"
                },
            ))
            .shortcut(if focused {
                super::keymap::label(super::keymap::Action::ToggleTileZoom)
            } else {
                String::new()
            })
            .on_select(move |s: &mut UiState| s.toggle_tile_zoom_at(i)),
        )
        .item(
            MenuItem::new(tr(
                language,
                if popped { "Dock pane" } else { "Pop out pane" },
            ))
            .shortcut(if focused {
                super::keymap::label(super::keymap::Action::ToggleTilePop)
            } else {
                String::new()
            })
            .on_select(move |s: &mut UiState| s.toggle_tile_pop_at(i)),
        )
        .item(
            MenuItem::new(tr(language, "Cycle tile layout")).on_select(|s: &mut UiState| {
                s.cycle_tile_layout();
            }),
        )
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
    let new_session = Button::new(tr_ref(language, "New session…"))
        .variant(ButtonVariant::Outline)
        .control_size(ControlSize::Regular)
        .on_press(move |s: &mut UiState| {
            s.pending_tile = Some(i);
            s.go(Mode::Home);
        })
        .build(k)
        .key(format!("tile-new-{i}"));

    // Icon grid for tile plugins: labels live in tooltips so many entries
    // wrap cleanly inside the same width as the session list.
    let mut plugins = row()
        .key("tile-pick-plugins")
        .w_full()
        .max_w(420.0)
        .gap(4.0)
        .flex_wrap()
        .justify_center()
        .center_y();
    for plugin in s.plugins.tile_plugins_enabled(&s.cfg.plugins_disabled) {
        let plugin_id = plugin.id();
        let plugin_title = plugin.title(language);
        let plugin_icon = plugin.icon();
        let pid = plugin_id.to_string();
        plugins = plugins.child(
            IconButton::new(plugin_icon)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Regular)
                .label(plugin_title.clone())
                .help(plugin_title)
                .on_press(move |s: &mut UiState| {
                    fill_plugin_tile(s, i, &pid);
                })
                .build(k)
                .key(format!("tile-plugin-{plugin_id}-{i}")),
        );
    }

    let actions = col()
        .key("tile-pick-actions")
        .w_full()
        .max_w(420.0)
        .gap(10.0)
        .items_center()
        .child(new_session)
        .child(plugins);

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
        .matched_crop()
        .hold_landing_width()
        .grow(1.0)
        // Panes share a cramped window by the split, rather than the first
        // giving all its room up to the others' floors.
        .min_w(0.0)
        .min_h(0.0)
        .radius(radius)
        .clip_rounded(radius)
        .fill(under)
        .border(
            1.0,
            if focused {
                k.c.bg_accent.alpha(0.65)
            } else {
                k.c.border_muted
            },
        )
        .motion(layout_motion(s))
        .transition(if look.motion {
            rune::Transition::scale(0.92).combined(rune::Transition::offset(0.0, 18.0))
        } else {
            rune::Transition::none()
        })
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
                surface: crate::plugin::PluginSlot::Tile,
            };
            if let Some(plugin) = s.plugins.get(kind) {
                if let Some(view) = crate::plugin::window::render(plugin.as_ref(), &ctx, layer) {
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
                surface: crate::plugin::PluginSlot::Tile,
            };
            if let Some(plugin) = s.plugins.get("diff") {
                if let Some(view) = crate::plugin::window::render(plugin.as_ref(), &ctx, layer) {
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
    body = body.child(
        look.energy_line(k, "window-energy", false)
            .pin(Some(12.0), Some(0.0), Some(12.0), None)
            .opacity(if focused { 1.0 } else { 0.12 })
            .opacity_motion(look.focus_fade()),
    );
    look.luminous(k, body, focused)
        .role("region")
        .label(format!(
            "{} {}",
            tr_ref(s.cfg.general.language, "Pane"),
            i + 1
        ))
}

fn header_tile_drag(s: &mut UiState, i: usize, d: &mut rune::Drag) {
    let floating = s.tile_layout_mode() == crate::config::TileLayoutMode::Floating
        || s.tiles.get(i).is_some_and(|t| t.popped);
    match d.phase {
        Phase::Down => {
            s.focus_tile(i);
            let start_rect = if floating {
                let rect = s
                    .tiles
                    .get(i)
                    .and_then(|t| t.floating_rect)
                    .unwrap_or_else(|| UiState::default_floating_rect(i, s.tile_grid));
                if let Some(tile) = s.tiles.get_mut(i) {
                    tile.floating_rect = Some(rect);
                }
                Some(fit_floating(rect, s.tile_grid))
            } else {
                None
            };
            s.tile_card_drag = Some(TileCardDrag {
                src: i,
                pointer: [0.0, 0.0],
                over: None,
                zone: None,
                start_rect,
            });
        }
        Phase::Move => {
            if floating {
                if let Some(start) = s.tile_card_drag.as_ref().and_then(|d| d.start_rect) {
                    let [sw, sh] = s.tile_grid;
                    let x = (start[0] + d.dx).clamp(0.0, (sw - 120.0).max(0.0));
                    let y = (start[1] + d.dy).clamp(0.0, (sh - 80.0).max(0.0));
                    if let Some(tile) = s.tiles.get_mut(i) {
                        tile.floating_rect =
                            Some(fit_floating([x, y, start[2], start[3]], [sw, sh]));
                    }
                }
            } else {
                // Approximate stage pointer from the tile's dwindle rect + local drag.
                let placed = placed_rects(s);
                if let Some((_, rect)) = placed.iter().find(|(idx, _)| *idx == i) {
                    let px = rect[0] + d.start_x + d.dx;
                    let py = rect[1] + d.start_y + d.dy;
                    if let Some(drag) = s.tile_card_drag.as_mut() {
                        drag.pointer = [px, py];
                    }
                    hit_test_tile_drag(s, px, py);
                }
            }
        }
        Phase::Up => {
            if let Some(drag) = s.tile_card_drag.take() {
                if !floating {
                    if let (Some(dst), Some(zone)) = (drag.over, drag.zone) {
                        s.drop_tile_on(drag.src, dst, zone);
                    }
                }
            }
        }
    }
}

fn hit_test_tile_drag(s: &mut UiState, px: f32, py: f32) {
    let Some(src) = s.tile_card_drag.as_ref().map(|d| d.src) else {
        return;
    };
    let hits = placed_rects(s);
    let mut over = None;
    let mut zone = None;
    for (i, rect) in hits {
        if i == src {
            continue;
        }
        let [x, y, rw, rh] = rect;
        if px >= x && px <= x + rw && py >= y && py <= y + rh {
            over = Some(i);
            zone = Some(dwindle::drop_zone(rect, px, py));
            break;
        }
    }
    if let Some(drag) = s.tile_card_drag.as_mut() {
        drag.over = over;
        drag.zone = zone;
        drag.pointer = [px, py];
    }
}

fn drop_preview_overlay(s: &UiState, k: &Kit) -> Option<El> {
    let drag = s.tile_card_drag.as_ref()?;
    let dst = drag.over?;
    let zone = drag.zone?;
    let rect = placed_rects(s).into_iter().find(|(i, _)| *i == dst)?.1;
    let [x, y, pw, ph] = dwindle::drop_preview_rect(rect, zone);
    Some(
        el().key("tile-drop-preview")
            .motion(layout_motion(s))
            .transition(rune::Transition::opacity())
            .paint_order(20)
            .absolute(x, y)
            .w(pw)
            .h(ph)
            .radius(k.radius.md)
            .fill(k.c.bg_accent.alpha(0.22))
            .border(1.0, k.c.bg_accent.alpha(0.55))
            .a11y_hidden(),
    )
}

fn floating_card(
    s: &UiState,
    k: &Kit,
    look: &Look,
    i: usize,
    scale: f32,
    layer: &mut Vec<El>,
) -> El {
    let rect = s
        .tiles
        .get(i)
        .and_then(|t| t.floating_rect)
        .unwrap_or_else(|| UiState::default_floating_rect(i, s.tile_grid));
    let [x, y, w, h] = fit_floating(rect, s.tile_grid);
    let card = tile(s, k, look, i, Place::default(), scale, layer);
    let resize_i = i;
    let handle = el()
        .key(format!("tile-float-resize-{i}"))
        .absolute(w - 14.0, h - 14.0)
        .size(14.0, 14.0)
        .cursor(Cursor::ResizeColumn)
        .fill(k.c.border_muted)
        .radius(3.0)
        .on_drag(move |s: &mut UiState, d: &mut rune::Drag, _: &mut Tx| {
            if d.phase == Phase::Down {
                s.focus_tile(resize_i);
                let start = s
                    .tiles
                    .get(resize_i)
                    .and_then(|t| t.floating_rect)
                    .unwrap_or([0.0, 0.0, 420.0, 320.0]);
                let start = fit_floating(start, s.tile_grid);
                d.data = start[2];
                if let Some(drag) = s.tile_card_drag.as_mut() {
                    drag.start_rect = Some(start);
                } else {
                    s.tile_card_drag = Some(TileCardDrag {
                        src: resize_i,
                        pointer: [0.0, 0.0],
                        over: None,
                        zone: None,
                        start_rect: Some(start),
                    });
                }
            } else if matches!(d.phase, Phase::Move | Phase::Up) {
                let start = s
                    .tile_card_drag
                    .as_ref()
                    .and_then(|d| d.start_rect)
                    .unwrap_or([0.0, 0.0, 420.0, 320.0]);
                let nw = (start[2] + d.dx).max(280.0);
                let nh = (start[3] + d.dy).max(180.0);
                if let Some(tile) = s.tiles.get_mut(resize_i) {
                    tile.floating_rect =
                        Some(fit_floating([start[0], start[1], nw, nh], s.tile_grid));
                }
                if d.phase == Phase::Up {
                    s.tile_card_drag = None;
                }
            }
        })
        .a11y_hidden();
    el().key(format!("tile-float-{i}"))
        .paint_order(if i == s.focus { 2 } else { 1 })
        .motion(layout_motion(s))
        .absolute(x, y)
        .w(w)
        .h(h)
        .child(card.w_full().h_full())
        .child(handle)
}

/// The gap between the two halves of the split at `path`: the window shows
/// through it, and dragging it moves the split. `room` is the length the
/// halves share along the split.
fn gap(s: &UiState, _k: &Kit, path: &[bool], across: bool, ratio: f32, room: f32) -> El {
    let key = path_key("tile-gap", path);
    let path_drag = path.to_vec();
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

    handle = if across {
        handle.w(GAP).h_full()
    } else {
        handle.h(GAP).w_full()
    };
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
            match (visible(a, s), visible(b, s)) {
                (true, false) | (false, true) => {
                    let take_b = !visible(a, s);
                    path.push(take_b);
                    let view = arrange(s, k, if take_b { b } else { a }, path, rect, cards);
                    path.pop();
                    return view;
                }
                _ => {}
            }
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

fn ghost_leaf(i: usize) -> El {
    el().key(format!("tile-ghost-{i}"))
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .a11y_hidden()
}

/// The panes, each its own card on the window with gaps between (the
/// main card steps aside for them); `clear` keeps the top-left header
/// clear of the window's corner ([`toggle_clearance`]).
pub fn view(s: &UiState, k: &Kit, clear: f32, scale: f32, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let mode = s.tile_layout_mode();
    let [w, h] = s.tile_grid;
    let size = [w.max(1.0), h.max(1.0)];

    let mut stage = el()
        .key("tiles-stage")
        .w_full()
        .h_full()
        .min_w(0.0)
        .min_h(0.0);

    // A stationary workbench makes floating motion legible against the desktop.
    // Gradients and a sparse grid use ordinary quads, without full-window blur.
    let mut backdrop = el()
        .key("workspace-depth")
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .clip_soft(0.0)
        .fill(k.c.bg_base)
        .child(look.light("workspace-aurora", [0.15, 0.2], [0.8, 0.8], k.c.bg_accent))
        .child(look.light(
            "workspace-horizon",
            [0.9, 0.85],
            [0.75, 0.65],
            k.c.info.icon,
        ))
        .a11y_hidden();
    let scheme = if s.dark {
        crate::theme::color_scheme::ColorScheme::Dark
    } else {
        crate::theme::color_scheme::ColorScheme::Light
    };
    if let (Some(images), Some(wallpaper)) = (
        s.images.as_ref(),
        crate::theme::apply::backdrop(scheme, false),
    ) {
        backdrop = backdrop.child(
            super::art::cover(
                images,
                &wallpaper,
                "workspace-theme-wallpaper",
                size[0],
                size[1],
            )
            .absolute(0.0, 0.0)
            .opacity(wallpaper.look.opacity),
        );
    }
    if mode == crate::config::TileLayoutMode::Floating && look.glass {
        for n in 1..24 {
            backdrop = backdrop.child(
                el().key(format!("workspace-grid-v-{n}"))
                    .absolute(size[0] * n as f32 / 24.0, 0.0)
                    .w(1.0)
                    .h_full()
                    .fill(k.c.bg_accent.alpha(0.045))
                    .a11y_hidden(),
            );
        }
        for n in 1..16 {
            backdrop = backdrop.child(
                el().key(format!("workspace-grid-h-{n}"))
                    .absolute(0.0, size[1] * n as f32 / 16.0)
                    .h(1.0)
                    .w_full()
                    .fill(k.c.bg_accent.alpha(0.045))
                    .a11y_hidden(),
            );
        }
    }
    stage = stage.child(backdrop);

    if let Some(z) = s.zoomed_tile.filter(|&i| i < s.tiles.len()) {
        stage = stage.child(tile(
            s,
            k,
            &look,
            z,
            Place {
                clear,
                corner: true,
            },
            scale,
            layer,
        ));
    } else {
        match mode {
            crate::config::TileLayoutMode::Scrolling => {
                let mut viewport = el().key("tiles-scrolling").w_full().h_full().clip_soft(0.0);
                let placed = scrolling_rects(s);
                for (i, [x, y, width, height]) in placed {
                    viewport = viewport.child(
                        tile(
                            s,
                            k,
                            &look,
                            i,
                            Place {
                                clear: if x <= 0.0 { clear } else { 0.0 },
                                corner: i == s.focus,
                            },
                            scale,
                            layer,
                        )
                        .absolute(x, y)
                        .w(width)
                        .h(height),
                    );
                }
                stage = stage.child(viewport);
            }

            crate::config::TileLayoutMode::Floating => {
                for i in 0..s.tiles.len() {
                    if s.tiles[i].popped {
                        continue;
                    }
                    stage = stage.child(floating_card(s, k, &look, i, scale, layer));
                }
            }
            crate::config::TileLayoutMode::Dwindle => {
                let tree = s.tile_layout();
                let placed = dwindle::rects(&tree, [0.0, 0.0, size[0], size[1]]);
                let top_left = placed
                    .iter()
                    .filter(|(i, _)| s.tiles.get(*i).is_some_and(|t| !t.popped))
                    .min_by(|x, y| (x.1[1] + x.1[0]).total_cmp(&(y.1[1] + y.1[0])))
                    .map(|p| p.0);
                let top_right = placed
                    .iter()
                    .filter(|(i, r)| r[1] < 1.0 && s.tiles.get(*i).is_some_and(|t| !t.popped))
                    .max_by(|x, y| x.1[0].total_cmp(&y.1[0]))
                    .map(|p| p.0);
                let mut cards: Vec<Option<El>> = (0..s.tiles.len())
                    .map(|i| {
                        if s.tiles[i].popped {
                            return Some(ghost_leaf(i));
                        }
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
                stage = stage.child(body);
            }
        }
    }

    // Popped tiles float above the tiled/scrolling body.
    if s.zoomed_tile.is_none() {
        for i in 0..s.tiles.len() {
            if s.tiles[i].popped {
                stage = stage.child(floating_card(s, k, &look, i, scale, layer));
            }
        }
    }
    if let Some(preview) = drop_preview_overlay(s, k) {
        stage = stage.child(preview);
    }

    // No toolbar: the panes rise under the window's empty toolbar row, so
    // their headers sit at the top, a frame's width from the window's edge.
    el().key("tiles-frame").w_full().grow(1.0).min_h(0.0).child(
        col()
            .key("tiles")
            .pin(
                Some(0.0),
                Some(-rune_kit::shell::TOOLBAR_HEIGHT),
                Some(0.0),
                Some(0.0),
            )
            .on_layout(|s: &mut UiState, rect: [f32; 4]| s.tile_grid = [rect[2], rect[3]])
            .on_pointer_move_local(|s: &mut UiState, ev, _| {
                if s.tile_card_drag.is_some()
                    && s.tile_layout_mode() != crate::config::TileLayoutMode::Floating
                {
                    hit_test_tile_drag(s, ev.x, ev.y);
                }
            })
            .child(stage),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Tile;

    #[test]
    fn scrolling_focus_reveals_readable_columns_and_uses_their_direction() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.tiles = vec![Tile::session("a"), Tile::session("b"), Tile::session("c")];
        s.tile_grid = [1000.0, 600.0];
        s.cfg.ui.tile_mode = crate::config::TileLayoutMode::Scrolling;
        s.focus = 1;
        let rects = scrolling_rects(&s);
        assert!(rects[1].1[0] >= 0.0 && rects[1].1[0] + rects[1].1[2] <= 1000.0);
        assert!(rects[1].1[2] >= 600.0);
        assert_eq!(neighbour(&s, &Key::Left), Some(0));
        assert_eq!(neighbour(&s, &Key::Right), Some(2));
        assert_eq!(neighbour(&s, &Key::Down), None);
        s.focus = 2;
        let end = scrolling_rects(&s)[2].1;
        assert_eq!(end[0] + end[2], 1000.0);
        s.tiles[1].popped = true;
        assert_eq!(scrolling_rects(&s).len(), 2);
    }

    #[test]
    fn floating_bounds_and_navigation_follow_visible_windows() {
        assert_eq!(
            fit_floating([900.0, 500.0, 600.0, 500.0], [800.0, 400.0]),
            [200.0, 0.0, 600.0, 400.0]
        );
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.tiles = vec![Tile::session("a"), Tile::session("b")];
        s.tile_grid = [1000.0, 700.0];
        s.cfg.ui.tile_mode = crate::config::TileLayoutMode::Floating;
        s.tiles[0].floating_rect = Some([500.0, 0.0, 300.0, 300.0]);
        s.tiles[1].floating_rect = Some([0.0, 0.0, 300.0, 300.0]);
        s.focus = 0;
        assert_eq!(neighbour(&s, &Key::Left), Some(1));
        assert_eq!(neighbour(&s, &Key::Right), None);
        s.cfg.ui.tile_mode = crate::config::TileLayoutMode::Dwindle;
        s.tiles[1].popped = true;
        let rects = placed_rects(&s);
        assert_eq!(
            rects.iter().find(|(i, _)| *i == 0).unwrap().1,
            [0.0, 0.0, 1000.0, 700.0]
        );
        s.tiles[1].popped = false;
        assert!(placed_rects(&s).iter().find(|(i, _)| *i == 0).unwrap().1[2] < 1000.0);
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

    #[test]
    fn omarchy_layout_zoom_pop_and_drop_zones() {
        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        s.open_session("s1");
        s.open_terminal_tile();
        assert_eq!(s.tile_layout_mode(), crate::config::TileLayoutMode::Dwindle);

        s.cycle_tile_layout();
        assert_eq!(
            s.tile_layout_mode(),
            crate::config::TileLayoutMode::Scrolling
        );
        s.cycle_tile_layout();
        assert_eq!(
            s.tile_layout_mode(),
            crate::config::TileLayoutMode::Floating
        );
        s.cycle_tile_layout();
        assert_eq!(s.tile_layout_mode(), crate::config::TileLayoutMode::Dwindle);

        s.toggle_tile_zoom();
        assert_eq!(s.zoomed_tile, Some(s.focus));
        s.toggle_tile_zoom();
        assert!(s.zoomed_tile.is_none());

        let focus = s.focus;
        s.toggle_tile_pop();
        assert!(s.tiles[focus].popped);
        assert!(s.tiles[focus].floating_rect.is_some());
        s.toggle_tile_pop();
        assert!(!s.tiles[focus].popped);

        s.split();
        assert_eq!(s.tiles.len(), 3);
        let before = format!("{:?}", s.tile_layout());
        s.drop_tile_on(2, 0, dwindle::DropZone::Left);
        let after = format!("{:?}", s.tile_layout());
        assert_ne!(before, after);

        s.toggle_tile_split_axis();
        s.equalize_tiles();
    }
}
