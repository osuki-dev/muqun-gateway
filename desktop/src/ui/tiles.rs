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

use rune::{col, el, row, text, Cursor, El, Key, Phase, Tx};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
    ScrollBar,
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
const HEAD_H: f32 = rune_kit::shell::TOOLBAR_HEIGHT;
/// How far an unfocused pane's content sits back: the alpha of the card
/// colour laid over it (light, dark), about a tenth of its contrast either
/// way (blending is in linear light, so dark needs more).
const UNFOCUSED_SCRIM: [f32; 2] = [0.12, 0.22];

/// `Primary+Alt+arrow` in split view: focus the pane that way; with Shift,
/// swap the focused pane's session with that pane's. Text fields pass it
/// here first (their own word moves take the plain chord).
pub fn move_toward(s: &mut UiState, key: &Key, swap: bool) -> bool {
    if !s.in_split() {
        return false;
    }
    let toward = neighbour(s, key);
    match toward {
        // The terminal lends its Ctrl chords with Shift added (Windows and
        // Linux), so from there the arrows only ever move focus.
        Some(j) if swap => {
            let i = s.focus;
            s.swap_tiles(i, j);
            true
        }
        Some(j) => {
            s.focus_tile(j);
            true
        }
        // An arrow toward the edge is still ours: it must not move the caret.
        None => matches!(key, Key::Left | Key::Right | Key::Up | Key::Down),
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
        rune::Spring::new(0.42, 0.14)
    }
}

/// Apply this to structural containers too: closing a split removes its parent,
/// so a transition on the leaf alone cannot keep the old subtree on screen.
fn panel_transition(s: &UiState) -> rune::Transition {
    if s.look().motion {
        rune::Transition::opacity()
            .combined(rune::Transition::scale(0.94))
            .combined(rune::Transition::offset(0.0, 24.0))
    } else {
        rune::Transition::none()
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
    s.tile_pick_query = rune::TextEdit::new("");
    s.tile_pick_selected = 0;
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
        let instance = format!("{plugin_id}-{}", s.next_token);
        s.next_token += 1;
        s.tiles[i].content = super::TileContent::Custom {
            id: instance.clone(),
            kind: plugin_id.to_string(),
            title,
        };
        s.mount_plugin_view(plugin_id, &instance);
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
            head = head.child(super::terminal_panel::tile_header_tabs(s, k, Some(i)));
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
            let plugin_icon = s
                .plugins
                .get(kind)
                .map(|p| p.render_icon(s, 14.0, k.c.text_accent))
                .unwrap_or_else(|| rune::icon(&super::icons::GRID, 14.0).color(k.c.text_accent));
            let raw_title = s
                .plugins
                .get(kind)
                .and_then(|p| p.window_title(s))
                .unwrap_or_else(|| title.clone());
            let clean_title = raw_title
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let kind_label = s
                .plugins
                .get(kind)
                .map(|p| p.title(language))
                .unwrap_or_else(|| kind.clone());
            head = head.child(plugin_icon.a11y_hidden()).child(
                text(clean_title.clone())
                    .key("tile-title")
                    .font(title_font)
                    .single_line()
                    .truncate()
                    .color(if focused {
                        k.c.text_base
                    } else {
                        k.c.text_muted
                    }),
            );
            if !kind_label.is_empty() && !kind_label.eq_ignore_ascii_case(&clean_title) {
                head = head.child(
                    text(kind_label)
                        .key("tile-agent")
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .single_line()
                        .truncate()
                        .shrink()
                        .min_w(0.0),
                );
            }
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
    // The complete title region is draggable, even when a long title leaves
    // no blank spacer. Action buttons stay outside this drag target.
    head = head.child(el().key("tile-spacer").grow(1.0).h_full().a11y_hidden());
    let drag_area = head
        .key(format!("tile-drag-region-{i}"))
        .w(0.0)
        .grow(1.0)
        .min_w(0.0);
    let drag_area = if s.in_split() {
        drag_area.cursor(Cursor::Grab).on_drag(
            move |s: &mut UiState, d: &mut rune::Drag, _: &mut Tx| header_tile_drag(s, i, d),
        )
    } else {
        drag_area.window_drag()
    };
    let mut head = row()
        .key("tile-head")
        .w_full()
        .h(HEAD_H)
        .pr(6.0)
        .gap(4.0)
        .center_y()
        .hover_group()
        .child(drag_area);
    // Move and close: visible under the pointer, always reachable by menu.
    let menu_open = s.menu == Some(OpenMenu::Tile(i));
    let mut menu = Menu::new(format!("tile-menu-{i}"))
        .min_width(320.0)
        .trigger(
            super::icons::header_button(&super::icons::MORE, tr_ref(language, "Pane actions"))
                .expanded(menu_open)
                .on_press(move |s: &mut UiState| {
                    s.menu = (!menu_open).then_some(OpenMenu::Tile(i));
                })
                .build(k)
                .key(format!("tile-menu-btn-{i}")),
        )
        .open(menu_open)
        .on_close(|s: &mut UiState| s.menu = None);
    let plugin_id = match content {
        super::TileContent::Custom { kind, .. } => Some(kind.as_str()),
        super::TileContent::Diff(_) => Some("diff"),
        super::TileContent::Terminal => Some("terminal"),
        _ => None,
    };
    let zoomed = s.zoomed_tile == Some(i);
    let popped = s.tiles.get(i).is_some_and(|t| t.popped);
    let pop_shortcut = focused.then(|| super::keymap::label(super::keymap::Action::ToggleTilePop));

    let mut has_section = false;
    if let Some(pid) = plugin_id {
        menu = crate::plugin::append_surface_items(
            menu,
            s,
            language,
            pid,
            crate::plugin::PluginSlot::Tile,
            pop_shortcut.clone(),
        );
        has_section = true;
    }

    // Swap places with a neighbour, as the panes sit.
    let tree = s.tile_layout();
    let chords = super::keymap::labels(super::keymap::Action::SwapPaneToward);
    let swaps: Vec<_> = [
        (Key::Left, "Swap with the pane on the left"),
        (Key::Right, "Swap with the pane on the right"),
        (Key::Up, "Swap with the pane above"),
        (Key::Down, "Swap with the pane below"),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(n, (key, en))| {
        super::dwindle::neighbour(&tree, s.tile_grid, i, &key).map(|j| (n, en, j))
    })
    .collect();
    if !swaps.is_empty() {
        if has_section {
            menu = menu.separator();
        }
        for (n, en, j) in swaps {
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
        has_section = true;
    }

    match content {
        super::TileContent::Session(a) => {
            if has_section {
                menu = menu.separator();
            }
            let a = a.clone();
            let fork_id = a.clone();
            let compact_id = a.clone();
            let busy = s.session_by_id(&a).is_some_and(|x| {
                matches!(
                    x.status,
                    muqun_gateway::agents::AgentSessionStatus::Busy
                        | muqun_gateway::agents::AgentSessionStatus::Retry
                )
            });
            menu = menu
                .item(MenuItem::new(tr(language, "Fork session")).on_select(
                    move |s: &mut UiState| {
                        s.menu = None;
                        s.send(Command::ForkSession {
                            asid: muqun_gateway::agents::AgentSessionId(fork_id.clone()),
                            message_id: None,
                        });
                    },
                ))
                .item(
                    MenuItem::new(tr(language, "Compact history"))
                        .disabled(busy)
                        .on_select(move |s: &mut UiState| {
                            s.menu = None;
                            s.send(Command::CompactSession {
                                asid: muqun_gateway::agents::AgentSessionId(compact_id.clone()),
                            });
                        }),
                )
                .separator()
                .item(MenuItem::new(tr(language, "Show only this one")).on_select(
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
            has_section = true;
        }
        super::TileContent::Terminal => {
            if has_section {
                menu = menu.separator();
            }
            menu = menu
                .item(
                    MenuItem::new(tr(language, "New terminal tab"))
                        .icon(&super::icons::PLUS)
                        .shortcut("Ctrl+Shift+T")
                        .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                            s.focus_terminal_tile_tx(Some(i), tx);
                            s.term_new_tab();
                            s.focus_terminal_tx(tx);
                        }),
                )
                .item(
                    MenuItem::new(tr(language, "Switch terminal to session folder"))
                        .icon(&rune_kit::glyphs::FOLDER)
                        .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                            s.focus_terminal_tile_tx(Some(i), tx);
                            s.term_sync_cwd();
                            s.focus_terminal_tx(tx);
                        }),
                )
                .item(
                    MenuItem::new(tr(language, "Restart terminal"))
                        .icon(&super::icons::REFRESH)
                        .on_select_tx(move |s: &mut UiState, tx: &mut Tx| {
                            s.focus_terminal_tile_tx(Some(i), tx);
                            s.restart_terminal();
                            s.focus_terminal_tx(tx);
                        }),
                )
                .separator()
                .item(MenuItem::new(tr(language, "Show only this one")).on_select(
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
            has_section = true;
        }
        super::TileContent::Custom { kind, .. } => {
            if let Some(plugin) = s.plugins.get(kind) {
                let cmds = plugin.commands();
                if !cmds.is_empty() {
                    if has_section {
                        menu = menu.separator();
                    }
                    for cmd in cmds {
                        let cmd_id = cmd.id;
                        let p = plugin.clone();
                        menu = menu.item(
                            MenuItem::new(cmd.title)
                                .shortcut(cmd.shortcut.unwrap_or_default())
                                .on_select(move |s: &mut UiState| {
                                    s.menu = None;
                                    p.on_command(s, cmd_id);
                                }),
                        );
                    }
                    has_section = true;
                }
            }
        }
        _ => {}
    }
    if has_section {
        menu = menu.separator();
    }
    menu = menu.item(
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
    );
    if plugin_id.is_none() {
        menu = menu.item(
            MenuItem::new(tr(
                language,
                if popped { "Dock pane" } else { "Pop out pane" },
            ))
            .shortcut(pop_shortcut.unwrap_or_default())
            .on_select(move |s: &mut UiState| s.toggle_tile_pop_at(i)),
        );
    }
    menu = menu
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
    // Unified window header trailing order across Tile, Float, Dock, and Page:
    // [contextual window actions] -> [... always-visible menu] -> [close].
    if matches!(content, super::TileContent::Session(_)) {
        let active_inline = s.tiles.get(i).and_then(|t| t.inline_panel);
        if crate::plugin::is_enabled("git", &s.cfg.plugins_disabled) {
            head = head.child(
                super::icons::header_button(&super::icons::BRANCH, tr_ref(language, "Git"))
                    .variant(if active_inline == Some(super::TileInlinePanel::Git) {
                        IconButtonVariant::Contrast
                    } else {
                        IconButtonVariant::GhostMuted
                    })
                    .help(tr_ref(language, "Git"))
                    .on_press(move |s: &mut UiState| {
                        s.toggle_tile_inline_panel(i, super::TileInlinePanel::Git);
                    })
                    .build(k)
                    .key(format!("tile-git-{i}")),
            );
        }
        if crate::plugin::is_enabled("activity", &s.cfg.plugins_disabled) {
            head = head.child(
                super::icons::header_button(
                    &super::icons::catalog::STACK,
                    tr_ref(language, "Activity"),
                )
                .variant(if active_inline == Some(super::TileInlinePanel::Activity) {
                    IconButtonVariant::Contrast
                } else {
                    IconButtonVariant::GhostMuted
                })
                .help(tr_ref(language, "Activity"))
                .on_press(move |s: &mut UiState| {
                    s.toggle_tile_inline_panel(i, super::TileInlinePanel::Activity);
                })
                .build(k)
                .key(format!("tile-activity-{i}")),
            );
        }
    }
    if let Some(plugin_id) = plugin_id {
        if let Some(actions) = s
            .plugins
            .get(plugin_id)
            .and_then(|p| p.window_actions(s, k))
        {
            head = head.child(actions);
        }
    }
    head = head.child(menu);
    head = head.child(
        super::icons::header_button(k.glyph.close, tr_ref(language, "Remove from split view"))
            .shortcut(if focused {
                super::keymap::label(super::keymap::Action::RemoveSplit)
            } else {
                String::new()
            })
            .on_press(move |s: &mut UiState| s.close_tile(i))
            .build(k)
            .key(format!("tile-close-{i}"))
            .group_opacity(if focused || popped { 0.85 } else { 0.55 }, 1.0),
    );
    // The split view has no toolbar: the top-right pane's header carries
    // what it held, adding a pane and toggling the right panel, always in view.
    if place.corner {
        head = head.child(window_actions(s, k));
    }
    head
}

/// Add to split view and the right panel toggle, at the trailing end of the
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
            super::icons::header_button(
                &super::icons::ADD_PANE,
                tr_ref(language, "Add to split view"),
            )
            .help(tip(tr_ref(language, "Add to split view"), Action::AddSplit))
            .on_press(|s: &mut UiState| s.split())
            .build(k)
            .key("split-add"),
        );
    }
    out.child(super::shell::right_panel_toggle(s, k))
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
            crate::ui::art::ActivityMark::Compact.build(k),
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
    let mut parked_draft = s.tiles[i].slot.draft.clone();
    parked_draft.focused = false;
    let editor = rune_kit::TextArea::new(&parked_draft)
        .placeholder(tr_ref(language, "Message…"))
        .label(tr_ref(language, "Draft"))
        .width(None)
        .auto_grow(1, 12)
        .build(&composer::bare_field(k))
        .key("draft")
        // This is a preview until its tile is active. Let the card receive the
        // first click instead of focusing an editor that is replaced on activation.
        .inert()
        .w_full();
    let mut actions = row()
        .gap(super::composer_row::ACTION_GAP)
        .w_full()
        .center_y()
        .child(
            IconButton::new(&super::parts::glyphs::ATTACH)
                .label(tr_ref(language, "Attach"))
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::ExtraLarge)
                .on_press(move |s: &mut UiState| {
                    s.focus_tile(i);
                    crate::platform::dialogs::attach_files(
                        s.current()
                            .and_then(|x| x.directory.as_ref())
                            .map(std::path::PathBuf::from),
                    );
                })
                .build(k),
        )
        .child(el().grow(1.0));
    if s.cfg.voice.usable() {
        actions = actions.child(
            IconButton::new(&super::voice::MIC)
                .label(tr_ref(language, "Dictate"))
                .variant(IconButtonVariant::Ghost)
                .control_size(ControlSize::ExtraLarge)
                .on_press(move |s: &mut UiState| {
                    s.focus_tile(i);
                    s.toggle_voice();
                })
                .build(k)
                .key("mic"),
        );
    }
    let submit = Button::new(tr_ref(language, "Send"))
        .label_with(|_, ink| rune::icon(&composer::SEND, composer::SEND_ICON).color(ink))
        .control_size(ControlSize::Regular)
        .variant(ButtonVariant::Submit)
        .disabled(
            s.pending_submit.is_some()
                || (s.tiles[i].slot.draft.text.trim().is_empty()
                    && s.tiles[i].slot.staged.is_empty()),
        )
        .on_press(move |s: &mut UiState| {
            s.focus_tile(i);
            composer::send_with_delivery(s);
        })
        .build(k)
        .size(36.0, 36.0)
        .key("send")
        .radius(k.radius.lg);
    actions = actions.child(submit);
    let card = col()
        .key(composer::card_layout_key(Some(asid), s.in_split()))
        .on_drop_files(move |s: &mut UiState, files, _| {
            s.focus_tile(i);
            composer::attach_dropped_files(s, files);
        })
        .on_drop_hover(move |s: &mut UiState, over, _| {
            if over {
                s.focus_tile(i);
            }
            s.drop_hover = over;
        })
        .on_background_press(move |s: &mut UiState| s.focus_tile(i))
        .w_full()
        .gap(4.0)
        .px(16.0)
        .pt(8.0)
        .pb(8.0)
        .child(editor)
        .child(actions);
    let footer = composer::session_footer(
        s,
        k,
        &mut Vec::new(),
        Some(asid),
        s.tiles[i].slot.catalog.as_deref(),
        &s.tiles[i].slot.diff_stat,
        Some(i),
    );
    col()
        .key("tile-composer-pad")
        .w_full()
        .items_center()
        .px(16.0)
        .pt(6.0)
        .pb(8.0)
        .gap(6.0)
        .child(
            composer::composer_card(k, look, card, false, false, false)
                .w_full()
                .max_w(super::timeline::FEED_MAX_W),
        )
        .child(footer.max_w(super::timeline::FEED_MAX_W))
}

fn picker_candidates<'a>(
    s: &'a UiState,
    open: &[&str],
    query: &str,
) -> Vec<&'a muqun_gateway::agents::AgentSessionInfo> {
    let needle = query.trim().to_ascii_lowercase();
    let mut rows: Vec<&muqun_gateway::agents::AgentSessionInfo> = s
        .sessions
        .iter()
        .filter(|x| !open.contains(&x.asid.0.as_str()))
        .filter(|x| !super::sessions::is_hidden_sidebar_session(s, x))
        .filter(|x| {
            if needle.is_empty() {
                return true;
            }
            let title = title_of(x).to_ascii_lowercase();
            let dir = x.directory.as_deref().unwrap_or("").to_ascii_lowercase();
            let agent = s.agent_name(&x.agent_id).to_ascii_lowercase();
            title.contains(&needle) || dir.contains(&needle) || agent.contains(&needle)
        })
        .collect();
    rows.sort_by_key(|info| std::cmp::Reverse(info.updated_ms));
    rows
}

/// An empty tile: search/pick a recent session, start a new one, or open a tool.
fn picker(s: &UiState, k: &Kit, look: &Look, i: usize) -> El {
    let language = s.cfg.general.language;
    let now = now_ms();
    let open: Vec<&str> = s.tiles.iter().filter_map(|t| t.asid.as_deref()).collect();
    let candidates = picker_candidates(s, &open, &s.tile_pick_query.text);
    let selected = if candidates.is_empty() {
        0
    } else {
        s.tile_pick_selected.min(candidates.len() - 1)
    };
    let match_ids: Vec<String> = candidates.iter().map(|c| c.asid.0.clone()).collect();

    let mut search_input = rune_kit::TextInput::new(&s.tile_pick_query)
        .placeholder(tr_ref(language, "Search sessions"))
        .leading_icon(&rune_kit::glyphs::SEARCH)
        .leading_icon_size(14.0)
        .width(None)
        .on_edit(|s: &mut UiState| {
            s.tile_pick_selected = 0;
            &mut s.tile_pick_query
        })
        .on_key({
            let ids = match_ids.clone();
            move |s: &mut UiState, ev: &rune::KeyEvent, _: &mut Tx| match ev.key {
                Key::Down if !ids.is_empty() => {
                    s.tile_pick_selected = (s.tile_pick_selected + 1) % ids.len();
                    true
                }
                Key::Up if !ids.is_empty() => {
                    s.tile_pick_selected = if s.tile_pick_selected == 0 {
                        ids.len() - 1
                    } else {
                        s.tile_pick_selected - 1
                    };
                    true
                }
                Key::Enter if !ev.mods.shift && !ids.is_empty() => {
                    let pick = s.tile_pick_selected.min(ids.len() - 1);
                    fill_tile(s, i, &ids[pick]);
                    true
                }
                Key::Escape if !s.tile_pick_query.text.is_empty() => {
                    s.tile_pick_query = rune::TextEdit::new("");
                    s.tile_pick_selected = 0;
                    true
                }
                _ => false,
            }
        });
    if !s.tile_pick_query.text.is_empty() {
        search_input = search_input
            .clear_button()
            .on_action(|s: &mut UiState, _: &mut Tx| {
                s.tile_pick_query = rune::TextEdit::new("");
                s.tile_pick_selected = 0;
            });
    }

    let mut list_inner = col().key("tile-pick").w_full().gap(5.0);
    if candidates.is_empty() {
        let msg = if s.tile_pick_query.text.trim().is_empty() {
            tr(language, "No other sessions yet")
        } else {
            tr(language, "No matching sessions")
        };
        list_inner = list_inner.child(
            row()
                .w_full()
                .py(20.0)
                .center()
                .radius(k.radius.lg)
                .border(1.0, k.c.border_weak)
                .fill(k.c.layer_01.alpha(if s.dark { 0.25 } else { 0.55 }))
                .child(text(msg).font(k.ty.small).color(k.c.text_faint)),
        );
    } else {
        for (idx, info) in candidates.iter().take(PICK_ROWS * 3).enumerate() {
            let asid = info.asid.0.clone();
            let activity = s.activity(info);
            let is_sel = idx == selected;
            let folder = info
                .directory
                .as_deref()
                .and_then(|d| std::path::Path::new(d).file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("");
            let subtitle = if folder.is_empty() {
                format!(
                    "{} · {}",
                    s.agent_name(&info.agent_id),
                    ago(info.updated_ms, now, language)
                )
            } else {
                format!(
                    "{} · {} · {}",
                    s.agent_name(&info.agent_id),
                    folder,
                    ago(info.updated_ms, now, language)
                )
            };
            let mut line = row()
                .key(format!("pick-{}", info.asid.0))
                .w_full()
                .h(46.0)
                .px(12.0)
                .gap(10.0)
                .center_y()
                .role("button")
                .label(title_of(info))
                .on_hover(move |s: &mut UiState| {
                    s.tile_pick_selected = idx;
                })
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
                            text(subtitle)
                                .font(k.ty.caption())
                                .color(k.c.text_faint)
                                .truncate(),
                        ),
                );
            if activity != Activity::Idle {
                line = line.child(activity_pill(k, look, activity, language));
            }
            let mut card = look.pressable(k, look.row_card(k, line, 10.0));
            if is_sel {
                card = look.accent_rim(k, card, k.c.bg_accent);
            }
            list_inner = list_inner.child(card);
        }
    }

    let list = list_inner
        .max_h(320.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k)
        .w_full();

    let new_session = Button::new(tr_ref(language, "New session…"))
        .variant(ButtonVariant::Submit)
        .control_size(ControlSize::Small)
        .on_press(move |s: &mut UiState| {
            s.pending_tile = Some(i);
            s.go(Mode::Home);
        })
        .build(k)
        .key(format!("tile-new-{i}"));

    let header = row()
        .w_full()
        .center_y()
        .gap(8.0)
        .child(
            text(tr(language, "Open a session in this pane"))
                .font(k.ty.body_medium)
                .color(k.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
        .child(new_session);

    // Labeled tool chips: readable at a glance instead of bare mystery icons.
    let mut plugins = row()
        .key("tile-pick-plugins")
        .w_full()
        .gap(6.0)
        .flex_wrap()
        .center_y();
    let enabled_plugins = s.plugins.tile_plugins_enabled(&s.cfg.plugins_disabled);
    for plugin in &enabled_plugins {
        let plugin_id = plugin.id();
        let plugin_title = plugin.title(language);
        let plugin_icon = plugin.render_icon(s, 14.0, k.c.icon_muted);
        let pid = plugin_id.to_string();
        let label_text = plugin_title.clone();
        plugins = plugins.child(
            k.interactive(
                row()
                    .key(format!("tile-plugin-{plugin_id}-{i}"))
                    .h(28.0)
                    .px(10.0)
                    .gap(6.0)
                    .center_y()
                    .radius(k.radius.md)
                    .border(1.0, k.c.border_weak)
                    .fill(k.c.layer_01.alpha(if s.dark { 0.42 } else { 0.78 }))
                    .role("button")
                    .label(plugin_title.clone())
                    .help(plugin_title)
                    .child(plugin_icon)
                    .child(text(label_text).font(k.ty.small).color(k.c.text_muted)),
            )
            .on_press(move |s: &mut UiState| {
                s.tile_pick_query = rune::TextEdit::new("");
                s.tile_pick_selected = 0;
                fill_plugin_tile(s, i, &pid);
            }),
        );
    }

    let mut content = col()
        .key("tile-pick-card")
        .w_full()
        .max_w(460.0)
        .gap(12.0)
        .child(header)
        .child(search_input.build(k).key(format!("tile-pick-search-{i}")))
        .child(list);

    if !enabled_plugins.is_empty() {
        content = content
            .child(el().w_full().h(1.0).fill(k.c.border_weak).a11y_hidden())
            .child(
                col()
                    .key("tile-pick-actions")
                    .w_full()
                    .gap(8.0)
                    .child(
                        text(tr(language, "Plugins"))
                            .font(k.ty.caption())
                            .color(k.c.text_faint),
                    )
                    .child(plugins),
            );
    }

    let content = if look.motion {
        content
            .transition(rune::Transition::opacity().combined(rune::Transition::scale(0.98)))
            .motion(look.focus_fade())
    } else {
        content
    };

    col()
        .key("tile-empty")
        .grow(1.0)
        .w_full()
        .min_h(0.0)
        .center()
        .px(20.0)
        .py(16.0)
        .child(content)
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

/// Hover activation leaves modal interactions and composition with their owner.
fn focus_hovered_tile(s: &mut UiState, i: usize, tx: &mut rune::Tx) {
    if i == s.focus
        || i >= s.tiles.len()
        || s.mode != Mode::Session
        || s.menu.is_some()
        || s.row_menu.is_some()
        || s.context_open.is_some()
        || s.zoom.is_some()
        || s.voice.is_some()
        || s.draft.preedit().is_some()
        || s.timeline_search.active
        || s.tile_drag.is_some()
        || s.tile_card_drag.is_some()
        || s.drop_hover
    {
        return;
    }
    s.focus_tile(i);
    s.focus_pane_input(tx);
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
        .motion(layout_motion(s))
        .transition(if look.motion {
            rune::Transition::scale(0.92).combined(rune::Transition::offset(0.0, 18.0))
        } else {
            rune::Transition::none()
        })
        .on_hover_change(move |s: &mut UiState, inside, tx| {
            if inside {
                focus_hovered_tile(s, i, tx);
            }
        })
        .on_background_press(move |s: &mut UiState| {
            if s.focus != i {
                s.focus_tile(i);
            }
        });
    body = look.panel(k, body, radius);
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
            body = body.child(super::terminal_panel::tile_body_only(
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
        (super::TileContent::Session(asid), true) => {
            let feed = if s.show_diff {
                timeline::changes(s, k)
            } else {
                timeline::view(s, k)
            };
            let feed = feed
                .on_drop_files(|s: &mut UiState, files, _| composer::attach_dropped_files(s, files))
                .on_drop_hover(|s: &mut UiState, over, _| s.drop_hover = over);
            let chat = col()
                .key(format!("tile-chat-{i}"))
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .min_h(0.0)
                .child(feed)
                .child(composer::view(s, k, layer));
            if let Some(panel) = s.tiles[i].inline_panel {
                body = body.child(
                    row()
                        .key(format!("tile-split-body-{i}"))
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .child(chat)
                        .child(super::git_panel::tile_inline_drawer(
                            s, k, layer, i, asid, panel, 320.0,
                        )),
                );
            } else {
                body = body.child(chat.w_full());
            }
        }
        (super::TileContent::Session(asid), false) => {
            let chat = col()
                .key(format!("tile-chat-{i}"))
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .min_h(0.0)
                .child(
                    timeline::session_view(s, k, asid)
                        .on_drop_files(move |s: &mut UiState, files, _| {
                            s.focus_tile(i);
                            composer::attach_dropped_files(s, files);
                        })
                        .on_drop_hover(move |s: &mut UiState, over, _| {
                            if over {
                                s.focus_tile(i);
                            }
                            s.drop_hover = over;
                        }),
                )
                .child(parked_composer(s, k, look, i));
            if let Some(panel) = s.tiles[i].inline_panel {
                body = body.child(
                    row()
                        .key(format!("tile-split-body-{i}"))
                        .w_full()
                        .grow(1.0)
                        .min_h(0.0)
                        .child(chat)
                        .child(super::git_panel::tile_inline_drawer(
                            s, k, layer, i, asid, panel, 320.0,
                        )),
                );
            } else {
                body = body.child(chat.w_full());
            }
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
    let split_focus = focused && s.tiles.len() > 1;
    let tile_gpu_aura = split_focus && look.glass && super::fx::fx_enabled();
    if tile_gpu_aura {
        let [c0, _, _] = look.highlight_colors(k);
        body = body.child(
            el().key("tile-focus-aura")
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .radius(radius)
                .effect(super::fx::TILE_AURA)
                .effect_data(super::fx::data(c0, 1.0))
                .a11y_hidden(),
        );
    }
    look.luminous(k, body, split_focus && !tile_gpu_aura)
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
                } else {
                    s.persist_tiles();
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
    let [c0, c1, _] = s.look().highlight_colors(k);
    Some(
        el().key("tile-drop-preview")
            .motion(layout_motion(s))
            .transition(rune::Transition::opacity())
            .paint_order(20)
            .absolute(x, y)
            .w(pw)
            .h(ph)
            .radius(k.radius.md)
            .linear_gradient(
                std::f32::consts::FRAC_PI_2,
                [
                    (0.0, c0.alpha(0.24)),
                    (0.55, c1.alpha(0.18)),
                    (1.0, c0.alpha(0.12)),
                ],
            )
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
        .radius(k.radius.xs)
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
                    s.persist_tiles();
                }
            }
        })
        .a11y_hidden();
    let mut floating = el()
        .key(format!("tile-float-{i}"))
        .transition(panel_transition(s))
        .radius(k.radius.lg);
    for shadow in k.shadow_layers(&k.elevation.overlay) {
        floating = floating.box_shadow(shadow);
    }
    floating
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
                .motion(layout_motion(s))
                .transition(panel_transition(s))
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
        .motion(layout_motion(s))
        .child_exit(panel_transition(s))
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
    // Tiled gaps reveal the same window backdrop as the sidebar gutter.
    // Transparent windows keep the desktop visible even between floating panes.
    if mode == crate::config::TileLayoutMode::Floating && !look.see_through {
        stage = stage.child(backdrop);
    }

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
    fn native_file_drop_targets_inactive_composer_once_and_survives_focus_change() {
        use crate::model::commands::Command;
        use crate::model::compose::{ComposeCommand, StagedFile};
        use rune::{Env, Runtime};
        for hover in [false, true] {
            let (tx, mut rx) = crate::model::commands::channel();
            let mut s = UiState::new(tx);
            crate::ui::scenes::scenes()
                .iter()
                .find(|(name, _)| *name == "split2")
                .unwrap()
                .1(&mut s);
            s.splash = None;
            s.focus_tile(0);
            let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            let mut cards: Vec<_> = rt
                .elements()
                .into_iter()
                .filter(|n| {
                    n.key
                        .as_deref()
                        .is_some_and(|k| k == "composer-card" || k.starts_with("composer-card-"))
                })
                .collect();
            cards.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
            let r = cards[1].rect;
            let at = Some((r[0] + 20.0, r[1] + 20.0));
            let files = vec![
                std::path::PathBuf::from("/tmp/report.pdf"),
                std::path::PathBuf::from("/tmp/photo.png"),
            ];
            // No ordinary pointer-move/click: OS drags have their own event route.
            if hover {
                rt.drag_files(files.clone(), at);
                for _ in 0..3 {
                    rt.frame(1.0 / 60.0);
                }
                assert_eq!(rt.state.focus, 1);
                assert!(rt.state.drop_hover);
            }
            rt.drop_files(files, at);
            rt.frame(1.0 / 60.0);
            assert!(!rt.state.drop_hover);
            let mut staged = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                if let Command::Compose(ComposeCommand::Stage { id, name, .. }) = cmd {
                    staged.push((id, name));
                }
            }
            assert_eq!(
                staged.len(),
                2,
                "one OS drop must stage each file exactly once"
            );
            assert!(
                rt.state.toasts.is_empty(),
                "upload not completed: no success toast"
            );
            rt.state.focus_tile(0);
            for (id, name) in staged {
                composer::finish_staging(
                    &mut rt.state,
                    id,
                    Ok(StagedFile {
                        draft_token: None,
                        path: format!("/uploads/{name}"),
                        image: name.ends_with("png"),
                        name,
                        size: 16,
                    }),
                );
            }
            assert!(rt.state.staged.is_empty());
            assert_eq!(rt.state.tiles[1].slot.staged.len(), 2);
            assert!(rt.state.tiles[1].slot.draft.text.contains("[Image #1]"));
        }
    }

    #[test]
    fn hovering_composer_allows_paste_without_click_and_keeps_controls_still() {
        use rune::{Env, Runtime};
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap()
            .1(&mut s);
        s.cfg.voice.url = "https://voice.example.test/transcribe".into();
        for session in &mut s.sessions {
            session.status = muqun_gateway::agents::AgentSessionStatus::Busy;
        }
        s.splash = None;
        s.focus_tile(0);
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.poll = Some(UiState::poll_frame);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        for target in [1, 0, 1] {
            let mut drafts: Vec<_> = rt
                .elements()
                .into_iter()
                .filter(|n| n.key.as_deref() == Some("draft"))
                .collect();
            drafts.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
            let rect = drafts[target].rect;
            let controls = |rt: &Runtime<UiState>| {
                let mut out: Vec<_> = rt
                    .elements()
                    .into_iter()
                    .filter(|n| matches!(n.key.as_deref(), Some("mic" | "stop" | "send")))
                    .map(|n| (n.key, n.rect))
                    .collect();
                out.sort_by(|a, b| a.1[0].total_cmp(&b.1[0]));
                out
            };
            let before = controls(&rt);
            rt.pointer_move(rect[0] + 20.0, rect[1] + 10.0);
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            assert_eq!(rt.state.focus, target);
            let old = rt.state.draft.text.clone();
            rt.paste("hover-paste");
            rt.frame(1.0 / 60.0);
            assert_ne!(
                rt.state.draft.text, old,
                "hovered composer must accept paste without a click"
            );
            let after = controls(&rt);
            assert_eq!(before.len(), after.len());
            for (before, after) in before.iter().zip(&after) {
                for axis in 0..4 {
                    assert!(
                        (before.1[axis] - after.1[axis]).abs() < 1.0,
                        "control moved on hover: {before:?} -> {after:?}"
                    );
                }
            }
        }
        let message = rt
            .elements()
            .into_iter()
            .find(|n| {
                n.text.as_deref().is_some_and(|t| t.starts_with("Reading")) && n.rect[0] > 600.0
            })
            .unwrap();
        rt.pointer_move(message.rect[0] + 10.0, message.rect[1] + 10.0);
        rt.pointer_down(message.rect[0] + 10.0, message.rect[1] + 10.0);
        rt.pointer_up(message.rect[0] + 10.0, message.rect[1] + 10.0);
        rt.frame(1.0 / 60.0);
        let draft = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some("draft") && n.rect[0] > 600.0)
            .unwrap();
        rt.pointer_move(draft.rect[0] + 20.0, draft.rect[1] + 10.0);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let before = rt.state.draft.text.clone();
        rt.paste("after-reading");
        rt.frame(1.0 / 60.0);
        assert_ne!(
            rt.state.draft.text, before,
            "returning from the timeline must restore composer paste focus"
        );
    }

    #[test]
    fn double_click_selects_message_words_in_split_pane() {
        use rune::{Env, Runtime};
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap()
            .1(&mut s);
        s.splash = None;
        s.focus_tile(0);
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.poll = Some(UiState::poll_frame);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let node = rt
            .elements()
            .into_iter()
            .find(|n| {
                n.text.as_deref().is_some_and(|t| t.starts_with("Reading")) && n.rect[0] > 600.0
            })
            .expect("message text visible");
        let [x, y, _, h] = node.rect;
        rt.pointer_move(x + 10.0, y + h.min(20.0) / 2.0);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.focus, 1);
        for _ in 0..2 {
            rt.pointer_down(x + 10.0, y + h.min(20.0) / 2.0);
            rt.pointer_up(x + 10.0, y + h.min(20.0) / 2.0);
            rt.frame(0.08);
        }
        assert_eq!(
            rt.selection().map(|s| s.text().to_owned()).as_deref(),
            Some("Reading")
        );
    }

    #[test]
    fn context_popup_is_anchored_to_its_own_split_and_closes_on_focus_change() {
        use rune::{Env, Runtime};
        let (tx, _) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap();
        scene(&mut s);
        s.splash = None;
        let catalog = std::sync::Arc::new(crate::hub::CatalogView {
            catalog: serde_json::from_value(serde_json::json!({
                "models": [{"id":"test-model", "name":"Test model", "provider_id":"test"}],
                "modes": [], "mcp": []
            }))
            .unwrap(),
        });
        s.catalog = Some(catalog.clone());
        s.tiles[1].slot.catalog = Some(catalog);
        s.focus_tile(1);
        let asid = s.selected.clone().unwrap();
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let trigger_key = format!("session-context-{asid}-trigger");
        let trigger = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some(&trigger_key))
            .unwrap();
        let [x, y, w, h] = trigger.rect;
        rt.pointer_down(x + w / 2.0, y + h / 2.0);
        rt.pointer_up(x + w / 2.0, y + h / 2.0);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        assert_eq!(rt.state.context_open.as_deref(), Some(asid.as_str()));
        let popup_key = format!("session-context-{asid}-content");
        let popup = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some(&popup_key))
            .unwrap();
        assert!(
            popup.rect[0] > 720.0,
            "right-hand context opened on the left: {:?}",
            popup.rect
        );
        assert!(
            popup.rect[1] + popup.rect[3] <= y + 1.0,
            "popup must open above its button"
        );
        assert!(popup.rect[3] < 450.0, "context should size to its content");
        rt.update(|s, _| s.focus_tile(0));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.context_open.is_none());
        rt.update(|s, _| s.focus_tile(1));
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let model_key = format!("model-{asid}-trigger");
        let model = rt
            .elements()
            .into_iter()
            .find(|n| n.key.as_deref() == Some(&model_key))
            .unwrap();
        let [x, y, w, h] = model.rect;
        rt.pointer_down(x + w / 2.0, y + h / 2.0);
        rt.pointer_up(x + w / 2.0, y + h / 2.0);
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }
        let choice = rt
            .elements()
            .into_iter()
            .find(|n| n.role == "menuitemradio" && n.label.as_deref() == Some("Test model"))
            .expect("the active split exposes its catalog option");
        assert!(
            choice.rect[0] > 720.0,
            "model menu anchored to the other session: {:?}",
            choice.rect
        );
        assert_eq!(rt.state.selected.as_deref(), Some(asid.as_str()));
        rt.update(|s, _| s.focus_tile(0));
        rt.frame(1.0 / 60.0);
        assert!(rt.state.menu.is_none());
        assert!(rt.find("model-filter").is_none());
    }

    #[test]
    fn pending_permission_can_be_answered_without_opening_the_inspector() {
        use muqun_gateway::agents::{PermissionDecision, PermissionRequest};
        use rune::{Env, Runtime};
        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap();
        scene(&mut s);
        s.splash = None;
        let asid = s.selected.clone().unwrap();
        let request: PermissionRequest = serde_json::from_value(serde_json::json!({
            "id": "pending-shell", "asid": asid, "action": "shell",
            "resources": [], "prompt": "Run a command", "source_tool_call_id": "folded-tool",
            "options": [{"index": 0, "label": "No", "decision": "deny"}]
        }))
        .unwrap();
        s.waiting_perms.insert(asid.clone(), vec![request]);
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let button = rt
            .elements()
            .into_iter()
            .find(|node| node.key.as_deref() == Some("perm-pending-shell-0"))
            .expect("pending authorization must be reachable beside the composer");
        let [x, y, w, h] = button.rect;
        assert!(w > 0.0 && h > 0.0 && y >= 0.0 && y + h <= 900.0);
        rt.pointer_down(x + w / 2.0, y + h / 2.0);
        rt.pointer_up(x + w / 2.0, y + h / 2.0);
        rt.frame(1.0 / 60.0);
        let mut answered = false;
        while let Ok(command) = rx.try_recv() {
            if let crate::model::commands::Command::ReplyPermission {
                asid: target,
                id,
                decision,
            } = command
            {
                assert_eq!(target.0, asid);
                assert_eq!(id, "pending-shell");
                assert_eq!(decision, PermissionDecision::Deny);
                answered = true;
            }
        }
        assert!(answered);
        assert!(rt.state.waiting_perms[&asid].is_empty());
    }

    #[test]
    fn narrow_busy_split_keeps_delivery_on_the_footer_line() {
        use rune::{Env, Runtime};
        for width in [1100.0, 1280.0, 1440.0] {
            let (tx, _) = crate::model::commands::channel();
            let mut s = UiState::new(tx);
            crate::ui::scenes::scenes()
                .iter()
                .find(|(name, _)| *name == "split2")
                .unwrap()
                .1(&mut s);
            s.splash = None;
            s.tile_tree = crate::config::TileNode::Split {
                across: true,
                ratio: 0.5,
                a: Box::new(crate::config::TileNode::Leaf(0)),
                b: Box::new(crate::config::TileNode::Leaf(1)),
            };
            s.cfg.general.language = crate::config::Language::ZhTw;
            for session in &mut s.sessions {
                session.status = muqun_gateway::agents::AgentSessionStatus::Busy;
            }
            let mut rt = Runtime::new(s, crate::ui::view, (width, 900.0), Env::new(true, 1.0));
            for _ in 0..60 {
                rt.frame(1.0 / 60.0);
            }
            let nodes = rt.elements();
            let footers: Vec<_> = nodes
                .iter()
                .filter(|n| n.key.as_deref() == Some("composer-footer"))
                .collect();
            assert_eq!(footers.len(), 2);
            for footer in footers {
                assert!(
                    footer.rect[3] <= 37.0,
                    "footer wrapped at {width}: {:?}",
                    footer.rect
                );
                let delivery = nodes
                    .iter()
                    .find(|n| {
                        n.key.as_deref() == Some("composer-delivery")
                            && n.rect[0] >= footer.rect[0]
                            && n.rect[0] < footer.rect[0] + footer.rect[2]
                    })
                    .expect("busy session retains delivery control");
                assert!(
                    delivery.rect[0] + delivery.rect[2] <= footer.rect[0] + footer.rect[2] + 1.0,
                    "delivery overflows at {width}"
                );
                let stop = nodes
                    .iter()
                    .find(|n| {
                        n.key.as_deref() == Some("stop")
                            && n.rect[0] >= footer.rect[0]
                            && n.rect[0] < footer.rect[0] + footer.rect[2]
                    })
                    .expect("busy session retains interrupt control");
                assert!(
                    stop.rect[0] >= delivery.rect[0] + delivery.rect[2] - 1.0
                        && stop.rect[0] + stop.rect[2] <= footer.rect[0] + footer.rect[2] + 1.0,
                    "stop must follow delivery inside the footer at {width}: {:?} {:?} {:?}",
                    delivery.rect,
                    stop.rect,
                    footer.rect
                );
                assert!(
                    (delivery.rect[1] - footer.rect[1]).abs() < 1.0,
                    "delivery moved below footer at {width}: {:?} {:?}",
                    footer.rect,
                    delivery.rect
                );
            }
        }
    }

    #[test]
    fn clicking_parked_composer_focuses_it_and_keeps_drafts_separate() {
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let (_, scene) = crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap();
        scene(&mut s);
        s.splash = None;
        s.draft = rune::TextEdit::new("first");
        s.tiles[1].slot.draft = rune::TextEdit::new("second");
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        rt.on_key = Some(crate::ui::on_key);
        for _ in 0..60 {
            rt.frame(1.0 / 60.0);
        }
        let sizes = |rt: &Runtime<UiState>| {
            let mut nodes: Vec<_> = rt
                .elements()
                .into_iter()
                .filter(|node| {
                    node.key.as_deref().is_some_and(|k| {
                        k == "composer-footer"
                            || k == "composer-card"
                            || k.starts_with("composer-card-")
                    })
                })
                .map(|node| (node.key.clone(), node.rect))
                .collect();
            nodes.sort_by(|a, b| a.1[0].total_cmp(&b.1[0]).then(a.0.cmp(&b.0)));
            nodes
        };
        let before = sizes(&rt);
        assert_eq!(before.len(), 4);
        for target in [1, 0] {
            let mut drafts: Vec<_> = rt
                .elements()
                .into_iter()
                .filter(|node| node.key.as_deref() == Some("draft"))
                .collect();
            drafts.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
            assert_eq!(drafts.len(), 2);
            let rect = drafts[target].rect;
            rt.pointer_down(rect[0] + 18.0, rect[1] + 16.0);
            rt.pointer_up(rect[0] + 18.0, rect[1] + 16.0);
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.focus, target);
            assert!(rt.state.draft.focused);
            rt.paste("!");
            rt.frame(1.0 / 60.0);
            assert_eq!(rt.state.draft.text.matches('!').count(), 1);
            for _ in 0..30 {
                rt.frame(1.0 / 60.0);
            }
            let after = sizes(&rt);
            for (old, new) in before.iter().zip(&after) {
                assert!(
                    (old.1[3] - new.1[3]).abs() < 1.0,
                    "focus changed composer height: {old:?} -> {new:?}"
                );
            }
        }
        assert_eq!(rt.state.draft.text.replace('!', ""), "first");
        assert_eq!(rt.state.tiles[1].slot.draft.text.replace('!', ""), "second");
        assert_eq!(rt.state.tiles[1].slot.draft.text.matches('!').count(), 1);
    }

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

    #[test]
    fn split_tile_session_menu_exposes_fork_and_compact_and_no_inline_inspector() {
        use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus};
        use rune::{Env, Runtime};

        let (tx, mut rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let s1: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-idle",
            "agent_id": "claude",
            "title": "Idle session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let s2: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-busy",
            "agent_id": "claude",
            "title": "Busy session",
            "status": "busy",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.sessions = vec![s1, s2];
        s.open_session("s-idle");
        s.open_in_tile("s-busy");
        s.focus_tile(0);
        s.splash = None;

        // Opening Git panel in a session tile opens an inline side drawer inside that tile.
        s.open_git_panel();
        assert!(!s.right_open);
        assert_eq!(
            s.tiles[0].inline_panel,
            Some(crate::ui::TileInlinePanel::Git)
        );
        assert!(s.tiles[1].inline_panel.is_none());

        s.menu = Some(OpenMenu::Tile(0));
        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        for _ in 0..10 {
            rt.frame(1.0 / 60.0);
        }
        let nodes = rt.elements();
        assert!(
            nodes
                .iter()
                .any(|n| n.key.as_deref() == Some("tile-inline-drawer-0")),
            "git panel must render inline inside tile 0"
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.key.as_deref() != Some("tile-inline-drawer-1")),
            "tile 1 must not open an inline drawer when only tile 0 opened git"
        );

        let click_item = |rt: &mut Runtime<UiState>, label: &str| {
            let item = rt
                .elements()
                .into_iter()
                .find(|n| n.role == "menuitem" && n.label.as_deref() == Some(label))
                .unwrap_or_else(|| panic!("missing menuitem {label}"));
            let [x, y, w, h] = item.rect;
            rt.pointer_down(x + w / 2.0, y + h / 2.0);
            rt.pointer_up(x + w / 2.0, y + h / 2.0);
            rt.frame(1.0 / 60.0);
        };

        while rx.try_recv().is_ok() {}
        click_item(&mut rt, "Fork session");
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Command::ForkSession { asid, message_id: None } if asid.0 == "s-idle"
            )),
            "expected ForkSession for s-idle, got {cmds:?}"
        );

        rt.update(|s, _| s.menu = Some(OpenMenu::Tile(0)));
        rt.frame(1.0 / 60.0);
        click_item(&mut rt, "Compact history");
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            cmds.iter().any(|cmd| matches!(
                cmd,
                Command::CompactSession { asid } if asid.0 == "s-idle"
            )),
            "expected CompactSession for s-idle, got {cmds:?}"
        );

        // On the busy tile, Compact history must be disabled.
        assert_eq!(rt.state.sessions[1].status, AgentSessionStatus::Busy);
        rt.update(|s, _| s.menu = Some(OpenMenu::Tile(1)));
        rt.frame(1.0 / 60.0);
        while rx.try_recv().is_ok() {}
        click_item(&mut rt, "Compact history");
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Command::CompactSession { .. })),
            "busy session must not trigger CompactSession, got {cmds:?}"
        );
    }

    #[test]
    fn narrow_split_composer_footer_controls_never_overlap_context_or_send() {
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        crate::ui::scenes::scenes()
            .iter()
            .find(|(name, _)| *name == "split2")
            .unwrap()
            .1(&mut s);
        s.splash = None;
        // Give both sessions long model names and branch info to stress narrow folding.
        for session in &mut s.sessions {
            session.model = Some(muqun_gateway::agents::ModelRef {
                provider_id: "anthropic".into(),
                model_id: "claude-opus-4-5-thinking".into(),
                variant: Some("high".into()),
            });
            if let Some(dir) = &session.directory {
                s.branches.insert(
                    dir.clone(),
                    crate::model::compose::BranchInfo {
                        current: Some("feat/very-long-branch-name".into()),
                        local: vec!["feat/very-long-branch-name".into(), "main".into()],
                        ..Default::default()
                    },
                );
            }
        }

        // 980px window with sidebar open => ~350px per split pane.
        let mut rt = Runtime::new(s, crate::ui::view, (980.0, 800.0), Env::new(true, 1.0));
        for _ in 0..30 {
            rt.frame(1.0 / 60.0);
        }

        let els = rt.elements();
        let card_keys: Vec<_> = els
            .iter()
            .filter_map(|n| n.key.as_deref())
            .filter(|k| k.starts_with("composer-card-"))
            .collect();
        assert_eq!(
            card_keys.len(),
            2,
            "each split tile must have its own unique composer-card-<asid> key, got {card_keys:?}"
        );
        assert_ne!(card_keys[0], card_keys[1]);

        let mut footers: Vec<_> = els
            .iter()
            .filter(|n| n.key.as_deref() == Some("composer-footer"))
            .collect();
        footers.sort_by(|a, b| a.rect[0].total_cmp(&b.rect[0]));
        assert_eq!(footers.len(), 2);

        let rects_overlap = |a: [f32; 4], b: [f32; 4]| {
            a[0] < b[0] + b[2] - 0.5
                && b[0] < a[0] + a[2] - 0.5
                && a[1] < b[1] + b[3] - 0.5
                && b[1] < a[1] + a[3] - 0.5
        };

        for footer in &footers {
            let fx0 = footer.rect[0] - 1.0;
            let fx1 = footer.rect[0] + footer.rect[2] + 1.0;
            let fy0 = footer.rect[1] - 2.0;
            let fy1 = footer.rect[1] + footer.rect[3] + 2.0;

            let in_footer = |r: [f32; 4]| {
                r[2] > 0.0
                    && r[3] > 0.0
                    && r[0] >= fx0
                    && r[0] + r[2] <= fx1 + 40.0
                    && r[1] >= fy0
                    && r[1] + r[3] <= fy1
            };

            let left_pills: Vec<_> = els
                .iter()
                .filter(|n| {
                    in_footer(n.rect)
                        && n.key.as_deref().is_some_and(|k| {
                            k == "composer-controls"
                                || k == "agent"
                                || k == "model-pill"
                                || k == "model-reload"
                                || k == "effort-pill"
                                || k == "branch"
                                || k == "mode-pill"
                                || k == "composer-more-pill"
                                || (k.starts_with("model-") && k.ends_with("-pill"))
                        })
                })
                .collect();

            let right_controls: Vec<_> = els
                .iter()
                .filter(|n| {
                    in_footer(n.rect)
                        && (n.key.as_deref() == Some("stop")
                            || n.key.as_deref() == Some("composer-git")
                            || n.label.as_deref() == Some("Context")
                            || n.key.as_deref().is_some_and(|k| k.starts_with("context-")))
                })
                .collect();

            let sends: Vec<_> = els
                .iter()
                .filter(|n| {
                    n.key.as_deref() == Some("send")
                        && n.rect[0] >= fx0
                        && n.rect[0] + n.rect[2] <= fx1
                })
                .collect();

            assert!(
                !left_pills.is_empty(),
                "expected left composer pills in footer {:?}",
                footer.rect
            );
            assert!(
                !right_controls.is_empty(),
                "expected right footer controls (Context/Stop) in footer {:?}",
                footer.rect
            );

            for left in &left_pills {
                for right in &right_controls {
                    assert!(
                        !rects_overlap(left.rect, right.rect),
                        "left pill {:?} ({:?}) overlapped right control {:?}/{:?} ({:?}) in narrow split footer {:?}",
                        left.key,
                        left.rect,
                        right.key,
                        right.label,
                        right.rect,
                        footer.rect
                    );
                }
                for send in &sends {
                    assert!(
                        !rects_overlap(left.rect, send.rect),
                        "left pill {:?} ({:?}) overlapped send button ({:?})",
                        left.key,
                        left.rect,
                        send.rect
                    );
                }
            }
        }
    }

    #[test]
    fn split_tiles_keep_independent_inline_git_and_activity_drawers() {
        use muqun_gateway::agents::AgentSessionInfo;
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let s1: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-left",
            "agent_id": "claude",
            "title": "Left session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let s2: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-right",
            "agent_id": "claude",
            "title": "Right session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.sessions = vec![s1, s2];
        s.open_session("s-left");
        s.open_in_tile("s-right");
        s.splash = None;

        s.toggle_tile_inline_panel(0, crate::ui::TileInlinePanel::Git);
        s.toggle_tile_inline_panel(1, crate::ui::TileInlinePanel::Activity);
        assert!(!s.right_open);
        assert_eq!(
            s.tiles[0].inline_panel,
            Some(crate::ui::TileInlinePanel::Git)
        );
        assert_eq!(
            s.tiles[1].inline_panel,
            Some(crate::ui::TileInlinePanel::Activity)
        );

        let mut rt = Runtime::new(s, crate::ui::view, (1600.0, 900.0), Env::new(true, 1.0));
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
        let nodes = rt.elements();
        let d0 = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("tile-inline-drawer-0"))
            .expect("left tile inline drawer");
        let d1 = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("tile-inline-drawer-1"))
            .expect("right tile inline drawer");
        assert!(
            d0.rect[0] + d0.rect[2] <= d1.rect[0] + 1.0,
            "left inline drawer ({:?}) must sit strictly to the left of right inline drawer ({:?})",
            d0.rect,
            d1.rect
        );
    }

    #[test]
    fn split_tile_right_pane_mention_popup_anchors_on_right_tile() {
        use muqun_gateway::agents::AgentSessionInfo;
        use rune::{Env, Runtime};

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let s1: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-left",
            "agent_id": "claude",
            "title": "Left session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        let s2: AgentSessionInfo = serde_json::from_value(serde_json::json!({
            "asid": "s-right",
            "agent_id": "claude",
            "title": "Right session",
            "status": "idle",
            "updated_ms": 1000u64,
        }))
        .unwrap();
        s.sessions = vec![s1, s2];
        s.open_session("s-left");
        s.open_in_tile("s-right");
        s.focus_tile(1);
        s.splash = None;
        s.draft = rune::TextEdit::new("@");
        s.draft.anchor = 1;
        s.draft.focus = 1;
        s.draft.focused = true;
        s.cfg.computers.insert(
            "mini".into(),
            crate::config::ComputerProfile {
                url: Some("http://100.83.99.56:23847".into()),
                token: Some("tok".into()),
                ..Default::default()
            },
        );

        let mut rt = Runtime::new(s, crate::ui::view, (1440.0, 900.0), Env::new(true, 1.0));
        for _ in 0..4 {
            rt.frame(1.0 / 60.0);
        }
        let nodes = rt.elements();
        let popup = nodes
            .iter()
            .find(|n| n.key.as_deref() == Some("mention-popup-panel"))
            .expect("mention popup should open for right split tile");
        assert!(
            popup.rect[0] > 600.0,
            "mention popup for right tile must anchor on the right tile, got {:?}",
            popup.rect
        );
    }

    #[test]
    fn picker_candidates_filter_hidden_sort_recent_and_match_query() {
        use muqun_gateway::agents::AgentSessionInfo;

        let (tx, _rx) = crate::model::commands::channel();
        let mut s = UiState::new(tx);
        let mk = |asid: &str, title: &str, dir: &str, updated_ms: u64, parent: Option<&str>| {
            let mut val = serde_json::json!({
                "asid": asid,
                "agent_id": "claude",
                "title": title,
                "directory": dir,
                "status": "idle",
                "updated_ms": updated_ms,
            });
            if let Some(p) = parent {
                val["parent_id"] = serde_json::json!(p);
            }
            serde_json::from_value::<AgentSessionInfo>(val).unwrap()
        };
        s.sessions = vec![
            mk("s-open", "Already Open", "/work/app", 5000, None),
            mk("s-old", "Refactor Auth", "/work/auth", 1000, None),
            mk("s-new", "Build Gateway", "/work/gateway", 4000, None),
            mk(
                "s-child",
                "Hidden Subagent",
                "/work/gateway",
                9000,
                Some("s-new"),
            ),
        ];

        let all = picker_candidates(&s, &["s-open"], "");
        let ids: Vec<&str> = all.iter().map(|c| c.asid.0.as_str()).collect();
        assert_eq!(ids, vec!["s-new", "s-old"]);

        let filtered = picker_candidates(&s, &["s-open"], "auth");
        let fids: Vec<&str> = filtered.iter().map(|c| c.asid.0.as_str()).collect();
        assert_eq!(fids, vec!["s-old"]);
    }
}
