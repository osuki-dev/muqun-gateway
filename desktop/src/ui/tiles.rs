//! Several sessions side by side: two to four tiles in one pane, each with
//! its session's live timeline and composer. One tile is focused (lit edge,
//! keyboard); a tile waiting on the owner pulses; tiles swap and close on
//! springs and the first column resizes on a grip.
//!
//! Keys: `Primary+1..4` focus a tile, `Primary+\` splits, `Primary+W` closes
//! the focused tile.

use rune::{col, el, row, text, Cursor, El, Phase, Tx};
use rune_kit::glyphs::{ARROW_RIGHT, CLOSE, TRIANGLE_DOWN};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Menu, MenuItem,
};

use super::copy::{tr, tr_ref};
use super::{
    activity_pill, ago, composer, now_ms, timeline, title_of, Activity, Mode, OpenMenu, UiState,
};
use crate::model::commands::Command;
use crate::theme::look::{Look, PANEL_RADIUS};

/// Space between tiles.
const GAP: f32 = 10.0;
/// The resize grip's width (it sits inside the gap).
const GRIP: f32 = 10.0;
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

/// The tile's header: mark, title, status, and its actions.
fn header(s: &UiState, k: &Kit, look: &Look, i: usize, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let focused = i == s.focus;
    let count = s.tiles.len();
    let asid = s.tiles[i].asid.clone();
    let info = asid.as_deref().and_then(|a| s.session_by_id(a));
    let mut head = row()
        .key("tile-head")
        .w_full()
        .h(40.0)
        .pl(12.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .hover_group();
    if let Some(info) = info {
        let activity = s.activity(info);
        head = head
            .child(super::agents::mark(&info.agent_id, 16.0, s.dark).a11y_hidden())
            .child(
                text(title_of(info))
                    .key("tile-title")
                    .font(k.ty.ui_medium)
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
                text(format!(
                    "{}{}",
                    s.agent_name(&info.agent_id),
                    info.model
                        .as_ref()
                        .map(|m| format!(" · {}", super::labels::format_model_name(m, "", None)))
                        .unwrap_or_default()
                ))
                .key("tile-agent")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate()
                .shrink()
                .min_w(0.0),
            );
        if activity != Activity::Idle {
            head = head.child(activity_pill(k, look, activity, language));
        }
    } else {
        head = head.child(
            text(tr(language, "Pick a session", "选择一个会话"))
                .key("tile-title")
                .font(k.ty.ui_medium)
                .color(k.c.text_muted),
        );
    }
    head = head.child(el().key("tile-spacer").grow(1.0).a11y_hidden());
    // A keycap that says how to reach the tile.
    head = head.child(
        row()
            .key("tile-key")
            .h(18.0)
            .px(5.0)
            .center()
            .radius(4.0)
            .fill(if focused {
                k.c.bg_accent.alpha(0.18)
            } else {
                k.c.layer_03
            })
            .border(
                1.0,
                if focused {
                    k.c.bg_accent.alpha(0.4)
                } else {
                    k.c.border_muted
                },
            )
            .child(
                text(crate::platform::keys::label(&format!("Primary+{}", i + 1)))
                    .font(k.ty.small_medium)
                    .color(if focused {
                        k.c.text_accent
                    } else {
                        k.c.text_faint
                    }),
            )
            .a11y_hidden(),
    );
    // Move and close: visible under the pointer, always reachable by menu.
    let menu_open = s.menu == Some(OpenMenu::Tile(i));
    let mut menu = Menu::new(format!("tile-menu-{i}"))
        .trigger(
            IconButton::new(&TRIANGLE_DOWN)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "Tile actions", "分屏操作"))
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
            MenuItem::new(tr(language, "Fill the pane", "铺满窗格")).on_select(
                move |s: &mut UiState| {
                    // Keep this session, drop the other tiles.
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
        MenuItem::new(tr(language, "Close tile", "关闭分屏"))
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
            .label(tr_ref(language, "Close tile", "关闭分屏"))
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
        .mx(12.0)
        .mb(12.0)
        .px(12.0)
        .gap(8.0)
        .center_y()
        .role("button")
        .label(tr_ref(language, "Focus this tile", "聚焦此分屏"))
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
    look.row_card(k, strip, 10.0)
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
                "Open a session in this tile",
                "在这个分屏打开一个会话",
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
    let agent_color = info.map(|i| super::agents::color(&i.agent_id, s.dark));

    let mut body = col()
        .key(id.clone())
        .matched(id.clone())
        .grow(1.0)
        .min_w(0.0)
        .min_h(0.0)
        .radius(PANEL_RADIUS)
        .clip_rounded(PANEL_RADIUS)
        .fill(k.c.bg_base)
        .motion(crate::theme::look::MOVE)
        .on_press(move |s: &mut UiState| s.focus_tile(i))
        .child(header(s, k, look, i, layer));
    // A thin lit line under the header in the agent's colour.
    if let Some(c) = agent_color {
        body = body.child(
            el().key("tile-rule")
                .w_full()
                .h(1.0)
                .linear_gradient(
                    std::f32::consts::FRAC_PI_2,
                    [
                        (0.0, c.alpha(0.0)),
                        (0.3, c.alpha(if focused { 0.7 } else { 0.3 })),
                        (0.7, c.alpha(if focused { 0.7 } else { 0.3 })),
                        (1.0, c.alpha(0.0)),
                    ],
                )
                .a11y_hidden(),
        );
    }
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
    // The edge: focused tiles are lit in the accent; a tile that needs the
    // owner pulses in the warning colour; the rest keep a hairline.
    body = if focused {
        look.glow(look.lit_edge(body, k.c.bg_accent), k.c.bg_accent, 0.55)
    } else {
        body.border(1.0, k.c.border_muted)
    };
    if activity == Activity::NeedsYou {
        body = body.child(
            look.pulse(
                el().key("tile-attention")
                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .radius(PANEL_RADIUS)
                    .border(1.5, k.c.warning.icon.alpha(0.9)),
            )
            .a11y_hidden(),
        );
    }
    body.role("region").label(format!(
        "{} {}",
        tr_ref(s.cfg.general.language, "Tile", "分屏"),
        i + 1
    ))
}

/// The grip between the first column and the rest: dragging it moves the split.
fn grip(s: &UiState, k: &Kit) -> El {
    let split = s.tile_split;
    el().key("tile-grip")
        .w(GRIP)
        .h_full()
        .cursor(Cursor::ResizeColumn)
        .role("separator")
        .label(tr_ref(s.cfg.general.language, "Resize split", "调整分栏"))
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
        .child(
            el().key("tile-grip-line")
                .w(3.0)
                .h(36.0)
                .radius(1.5)
                .fill(k.c.border_strong)
                .self_center()
                .group_opacity(0.35, 1.0)
                .a11y_hidden(),
        )
        .center()
}

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let n = s.tiles.len();
    let split = s.tile_split;
    let mut tiles: Vec<El> = (0..n).map(|i| tile(s, k, &look, i, layer)).collect();
    // Columns: the first tile (or first two, stacked) take `split` of the width.
    let first = |e: El| e.w_frac(split).shrink().min_w(0.0);
    let body = match n {
        2 => {
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            row()
                .key("tiles-row")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(GAP - GRIP)
                .child(first(a))
                .child(grip(s, k))
                .child(b)
        }
        3 => {
            let c = tiles.pop().unwrap();
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            row()
                .key("tiles-row")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(GAP - GRIP)
                .child(first(a))
                .child(grip(s, k))
                .child(
                    col()
                        .key("tiles-right")
                        .grow(1.0)
                        .min_w(0.0)
                        .min_h(0.0)
                        .gap(GAP)
                        .child(b)
                        .child(c),
                )
        }
        _ => {
            let d = tiles.pop().unwrap();
            let c = tiles.pop().unwrap();
            let b = tiles.pop().unwrap();
            let a = tiles.pop().unwrap();
            row()
                .key("tiles-row")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .gap(GAP - GRIP)
                .child(
                    col()
                        .key("tiles-left")
                        .w_frac(split)
                        .shrink()
                        .min_w(0.0)
                        .min_h(0.0)
                        .gap(GAP)
                        .child(a)
                        .child(c),
                )
                .child(grip(s, k))
                .child(
                    col()
                        .key("tiles-right")
                        .grow(1.0)
                        .min_w(0.0)
                        .min_h(0.0)
                        .gap(GAP)
                        .child(b)
                        .child(d),
                )
        }
    };
    col()
        .key("tiles")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .padding(GAP)
        .fill(k.c.bg_deep.alpha(if look.glass { 0.5 } else { 1.0 }))
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
        text(format!(
            "{} · {n} {}",
            tr_ref(language, "Split view", "分屏"),
            tr_ref(language, "sessions", "个会话")
        ))
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
            Button::new(tr_ref(language, "Add tile", "添加分屏"))
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
        Button::new(tr_ref(language, "Unsplit", "取消分屏"))
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
