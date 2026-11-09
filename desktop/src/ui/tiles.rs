//! Split view: two to four sessions side by side in the main card, each a
//! pane with its live timeline and composer. The panes fill the card edge to
//! edge with hairline dividers; the focused one's header is tinted and
//! underlined in the accent, one waiting on the owner pulses its underline;
//! panes swap and close on springs and the first column resizes on a grip.
//!
//! Keys: `Primary+1..4` and `Primary+Alt+arrows` focus a pane,
//! `Primary+\` adds one, `Primary+W` removes the focused one.

use rune::{col, el, row, text, Cursor, El, Key, KeyEvent, Phase, Tx};
use rune_kit::glyphs::{ARROW_RIGHT, CLOSE, TRIANGLE_DOWN};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
};

use super::copy::{tr, tr_ref};
use super::{
    activity_mark, activity_pill, ago, composer, now_ms, timeline, title_of, Activity, Mode,
    OpenMenu, UiState,
};
use crate::model::commands::Command;
use crate::theme::look::Look;

/// The resize grip's width; the divider line runs down its middle.
const GRIP: f32 = 9.0;
/// A pane header's height.
const HEAD_H: f32 = 36.0;

/// Which pane lies `key`-ward of pane `i` among `n`, as the panes sit: two
/// side by side; three as one on the left beside two stacked; four in a
/// two-by-two grid, numbered across then down.
pub fn neighbour(n: usize, i: usize, key: &Key) -> Option<usize> {
    // (column, row) of each pane; the lone left pane of three spans both rows.
    let at = |j: usize| -> (usize, usize) {
        match (n, j) {
            (3, 0) => (0, 0),
            (3, j) => (1, j - 1),
            (_, j) => (j % 2, j / 2),
        }
    };
    let (c, r) = at(i);
    let want = match key {
        Key::Left if c > 0 => (c - 1, r),
        Key::Right if c == 0 => (1, r),
        Key::Up if r > 0 => (c, r - 1),
        Key::Down => (c, r + 1),
        _ => return None,
    };
    (0..n).find(|&j| j != i && (at(j) == want || (n == 3 && j == 0 && want.0 == 0)))
}

/// `Primary+Alt+arrow` in split view: focus the pane that way. Text fields
/// pass it here first (their own word moves take the plain chord).
pub fn arrow_key(s: &mut UiState, ev: &KeyEvent) -> bool {
    if !s.in_split() || !ev.mods.primary() || !ev.mods.alt || ev.mods.shift {
        return false;
    }
    match neighbour(s.tiles.len(), s.focus, &ev.key) {
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
    if s.tiles.get(i).is_none_or(|t| t.asid.is_some()) {
        return;
    }
    if let Some(open) = s.tile_of(asid) {
        s.focus_tile(open);
        return;
    }
    s.tiles[i].asid = Some(asid.to_string());
    s.tiles[i].slot = super::SessionSlot {
        draft: rune::TextEdit::new(s.cfg.general.drafts.get(asid).cloned().unwrap_or_default()),
        ..Default::default()
    };
    s.send(Command::OpenTile(muqun_gateway::agents::AgentSessionId(
        asid.to_string(),
    )));
    s.focus_tile(i);
    s.persist_tiles();
}

/// How to reach pane `i` from the keyboard, for its header's tooltip.
fn reach(language: crate::config::Language, i: usize) -> String {
    format!(
        "{} · {} {}",
        crate::platform::keys::label(&format!("Primary+{}", i + 1)),
        crate::platform::keys::label("Primary+Alt"),
        tr_ref(
            language,
            "+ arrows to move between panes",
            "+ 方向键在窗格间切换"
        ),
    )
}

/// A pane's header, one line in the toolbar's style: mark, title, agent,
/// status dot, and under the pointer its menu and close.
fn header(s: &UiState, k: &Kit, look: &Look, i: usize, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let focused = i == s.focus;
    let count = s.tiles.len();
    let asid = s.tiles[i].asid.clone();
    let info = asid.as_deref().and_then(|a| s.session_by_id(a));
    let mut head = row()
        .key("tile-head")
        .w_full()
        .h(HEAD_H)
        .pl(12.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .help(reach(language, i))
        .hover_group();
    if focused {
        head = head.fill(k.c.bg_accent.alpha(0.07));
    }
    let title_font = if focused { k.ty.ui_medium } else { k.ty.ui };
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
                    .truncate()
                    .shrink()
                    .min_w(0.0),
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
        if activity != Activity::Idle {
            head = head.child(activity_mark(k, look, activity, language, false));
        }
    } else {
        head = head.child(
            text(tr(language, "Pick a session", "选择一个会话"))
                .key("tile-title")
                .font(title_font)
                .color(k.c.text_muted),
        );
    }
    head = head.child(el().key("tile-spacer").grow(1.0).a11y_hidden());
    // Move and close: visible under the pointer, always reachable by menu.
    let menu_open = s.menu == Some(OpenMenu::Tile(i));
    let mut menu = Menu::new(format!("tile-menu-{i}"))
        .trigger(
            IconButton::new(&TRIANGLE_DOWN)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Pane actions", "窗格操作"))
                .expanded(menu_open)
                .on_press(move |s: &mut UiState| {
                    s.menu = (!menu_open).then_some(OpenMenu::Tile(i));
                })
                .build(k)
                .key(format!("tile-menu-btn-{i}")),
        )
        .open(menu_open)
        .on_close(|s: &mut UiState| s.menu = None);
    if i > 0 {
        menu = menu.item(
            MenuItem::new(tr(language, "Move left / up", "左移 / 上移"))
                .on_select(move |s: &mut UiState| s.swap_tiles(i, i - 1)),
        );
    }
    if i + 1 < count {
        menu = menu.item(
            MenuItem::new(tr(language, "Move right / down", "右移 / 下移"))
                .on_select(move |s: &mut UiState| s.swap_tiles(i, i + 1)),
        );
    }
    if let Some(a) = asid.clone() {
        menu = menu.item(
            MenuItem::new(tr(language, "Show only this one", "只留这一个")).on_select(
                move |s: &mut UiState| {
                    // Keep this session, drop the other panes.
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
            ),
        );
    }
    menu = menu.separator().item(
        MenuItem::new(tr(language, "Remove from split view", "移出分屏"))
            .on_select(move |s: &mut UiState| s.close_tile(i)),
    );
    let menu = menu.build(k, layer);
    head = head.child(if menu_open {
        menu
    } else {
        menu.group_opacity(0.0, 1.0)
    });
    head.child(
        IconButton::new(&CLOSE)
            .variant(IconButtonVariant::GhostMuted)
            .control_size(ControlSize::Small)
            .label(tr_ref(language, "Remove from split view", "移出分屏"))
            .shortcut(if focused { "Primary+W" } else { "" })
            .on_press(move |s: &mut UiState| s.close_tile(i))
            .build(k)
            .key(format!("tile-close-{i}"))
            .group_opacity(if focused { 0.7 } else { 0.0 }, 1.0),
    )
}

/// A quiet one-line composer for an unfocused tile: shows its draft and
/// brings the tile into focus when pressed.
fn parked_composer(s: &UiState, k: &Kit, look: &Look, i: usize) -> El {
    let language = s.cfg.general.language;
    let draft = s.tiles[i].slot.draft.text.trim().to_string();
    let (line, ink) = if draft.is_empty() {
        (
            tr(language, "Click to type here…", "点击在此输入…"),
            k.c.text_faint,
        )
    } else {
        (draft, k.c.text_muted)
    };
    let queued = s.tiles[i].slot.queue.len();
    let mut strip = row()
        .key("tile-composer")
        .w_full()
        .h(44.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .role("button")
        .label(tr_ref(language, "Focus this pane", "聚焦此窗格"))
        .cursor(Cursor::Text)
        .on_press(move |s: &mut UiState| s.focus_tile(i))
        .child(
            text(line)
                .font(k.ty.ui)
                .color(ink)
                .truncate()
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        );
    if queued > 0 {
        strip = strip.child(
            rune_kit::Badge::new(format!("{queued} {}", tr_ref(language, "queued", "排队")))
                .variant(rune_kit::BadgeVariant::Subtle)
                .build(k),
        );
    }
    col()
        .key("tile-composer-pad")
        .w_full()
        .px(12.0)
        .pb(12.0)
        .child(look.row_card(k, strip, 10.0))
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
    col()
        .key("tile-empty")
        .grow(1.0)
        .w_full()
        .min_h(0.0)
        .center()
        .gap(16.0)
        .px(20.0)
        .child(
            text(tr(
                language,
                "Open a session in this pane",
                "在这个窗格打开一个会话",
            ))
            .font(k.ty.body_medium)
            .color(k.c.text_base),
        )
        .child(list)
        .child(
            Button::new(tr_ref(language, "New session…", "新建会话…"))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Regular)
                .on_press(move |s: &mut UiState| {
                    s.pending_tile = Some(i);
                    s.go(Mode::Home);
                })
                .build(k)
                .key(format!("tile-new-{i}")),
        )
}

fn tile(s: &UiState, k: &Kit, look: &Look, i: usize, layer: &mut Vec<El>) -> El {
    let focused = i == s.focus;
    let t = &s.tiles[i];
    let id = t
        .asid
        .clone()
        .map_or_else(|| format!("tile-empty-{i}"), |a| format!("tile-{a}"));
    let info = t.asid.as_deref().and_then(|a| s.session_by_id(a));
    let activity = info.map_or(Activity::Idle, |i| s.activity(i));

    let mut body = col()
        .key(id.clone())
        .matched(id.clone())
        .grow(1.0)
        // Panes share a cramped card by the split, rather than the first
        // giving all its room up to the others' floors.
        .min_w(0.0)
        .min_h(0.0)
        .clip_soft(0.0)
        .motion(crate::theme::look::MOVE)
        .on_press(move |s: &mut UiState| s.focus_tile(i))
        .child(header(s, k, look, i, layer));
    body = body.child(
        el().key("tile-rule")
            .w_full()
            .h(1.0)
            .fill(k.c.border_muted)
            .a11y_hidden(),
    );
    match (&t.asid, focused) {
        (None, _) => body = body.child(picker(s, k, look, i)),
        (Some(_), true) => {
            let feed = if s.show_diff {
                timeline::changes(s, k)
            } else {
                timeline::view(s, k)
            };
            body = body.child(feed).child(composer::view(s, k, layer));
        }
        (Some(asid), false) => {
            body = body
                .child(timeline::session_view(s, k, asid))
                .child(parked_composer(s, k, look, i));
        }
    }
    // Emphasis is light from the pane's top edge, never an outline: the
    // accent sweeping in on the focused pane, the warning colour breathing
    // on one waiting on the owner.
    if activity == Activity::NeedsYou {
        body = body.child(look.pulse(look.top_light("tile-attention", k.c.warning.icon, 0.9)));
    } else if focused {
        body = body.child(look.top_light("tile-focus-light", k.c.bg_accent, 1.0));
    }
    body.role("region").label(format!(
        "{} {}",
        tr_ref(s.cfg.general.language, "Pane", "窗格"),
        i + 1
    ))
}

/// A hairline between two stacked panes.
fn divider(k: &Kit, key: &str) -> El {
    el().key(key.to_string())
        .w_full()
        .h(1.0)
        .fill(k.c.border_muted)
        .a11y_hidden()
}

/// The grip between the first column and the rest: dragging it moves the split.
fn grip(s: &UiState, k: &Kit) -> El {
    let split = s.tile_split;
    el().key("tile-grip")
        .w(GRIP)
        .h_full()
        .cursor(Cursor::ResizeColumn)
        .role("separator")
        .label(tr_ref(
            s.cfg.general.language,
            "Resize split view",
            "调整分屏",
        ))
        .a11y_value(format!("{:.0}%", split * 100.0))
        .hover_group()
        .on_drag(
            move |s: &mut UiState, d: &mut rune::Drag, _: &mut Tx| match d.phase {
                Phase::Down => d.data = s.tile_split,
                Phase::Move => {
                    if s.tile_grid_w > 0.0 {
                        s.set_tile_split(d.data + d.dx / s.tile_grid_w);
                    }
                }
                Phase::Up => s.persist_tiles(),
            },
        )
        // The divider, brightening under the pointer.
        .child(
            el().key("tile-grip-line")
                .w(1.0)
                .h_full()
                .fill(k.c.border_muted)
                .a11y_hidden(),
        )
        .child(
            el().key("tile-grip-lit")
                .absolute((GRIP - 1.0) * 0.5, 0.0)
                .w(1.0)
                .h_full()
                .fill(k.c.border_strong)
                .group_opacity(0.0, 1.0)
                .a11y_hidden(),
        )
        .center()
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let n = s.tiles.len();
    let split = s.tile_split;
    let mut tiles: Vec<El> = (0..n).map(|i| tile(s, k, &look, i, layer)).collect();
    // Columns: the first pane (or first two, stacked) take `split` of the
    // width; the panes meet on hairlines, the vertical one the grip.
    let first = |e: El| e.w_frac(split).shrink().min_w(0.0);
    let stack = |key: &str, top: El, bottom: El| {
        col()
            .key(key.to_string())
            .min_w(0.0)
            .min_h(0.0)
            .child(top)
            .child(divider(k, &format!("{key}-divider")))
            .child(bottom)
    };
    let line = row().key("tiles-row").w_full().grow(1.0).min_h(0.0);
    let body = match n {
        2 => {
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            line.child(first(a)).child(grip(s, k)).child(b)
        }
        3 => {
            let c = tiles.pop().unwrap();
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            line.child(first(a))
                .child(grip(s, k))
                .child(stack("tiles-right", b, c).grow(1.0))
        }
        _ => {
            let d = tiles.pop().unwrap();
            let c = tiles.pop().unwrap();
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            line.child(first(stack("tiles-left", a, c)))
                .child(grip(s, k))
                .child(stack("tiles-right", b, d).grow(1.0))
        }
    };
    col()
        .key("tiles")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .on_layout(|s: &mut UiState, rect: [f32; 4]| s.tile_grid_w = rect[2])
        .child(body)
}

/// The toolbar's leading content while the pane is split.
pub fn toolbar_title(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let n = s.tiles.len();
    let working = s
        .tiles
        .iter()
        .filter_map(|t| t.asid.as_deref())
        .filter_map(|a| s.session_by_id(a))
        .filter(|i| s.activity(i) == Activity::Working)
        .count();
    let mut line = row().key("split-title").gap(8.0).center_y().child(
        text(format!("{} · {n}", tr_ref(language, "Split view", "分屏")))
            .font(k.ty.ui_medium)
            .color(k.c.text_base),
    );
    if working > 0 {
        line = line.child(
            text(format!(
                "{working} {}",
                tr_ref(language, "working", "工作中")
            ))
            .font(k.ty.small)
            .color(k.c.text_accent),
        );
    }
    line
}

/// The toolbar's trailing actions while the pane is split.
pub fn toolbar_actions(s: &UiState, k: &Kit) -> Vec<El> {
    let language = s.cfg.general.language;
    let mut out = Vec::new();
    if s.tiles.len() < super::MAX_TILES {
        out.push(
            Button::new(tr_ref(language, "Add to split view", "加入分屏"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .icon(&ARROW_RIGHT)
                .shortcut("Primary+\\")
                .on_press(|s: &mut UiState| s.split())
                .build(k)
                .key("split-add"),
        );
    }
    out.push(
        Button::new(tr_ref(language, "Close split view", "退出分屏"))
            .variant(ButtonVariant::Ghost)
            .control_size(ControlSize::Small)
            .on_press(|s: &mut UiState| {
                let mut j = s.tiles.len();
                while j > 0 {
                    j -= 1;
                    if j != s.focus && s.tiles.len() > 1 {
                        s.close_tile(j);
                    }
                }
            })
            .build(k)
            .key("split-unsplit"),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_move_between_panes_as_they_sit() {
        use Key::{Down, Left, Right, Up};
        let cases = [
            // Two side by side.
            (2, 0, Right, Some(1)),
            (2, 1, Left, Some(0)),
            (2, 0, Down, None),
            // One on the left beside two stacked.
            (3, 0, Right, Some(1)),
            (3, 2, Left, Some(0)),
            (3, 1, Down, Some(2)),
            (3, 2, Up, Some(1)),
            (3, 0, Down, None),
            // Two by two, numbered across then down.
            (4, 0, Down, Some(2)),
            (4, 2, Right, Some(3)),
            (4, 3, Up, Some(1)),
            (4, 1, Left, Some(0)),
            (4, 3, Right, None),
        ];
        for (n, i, key, want) in cases {
            assert_eq!(neighbour(n, i, &key), want, "{n} panes, from {i}, {key:?}");
        }
    }
}
