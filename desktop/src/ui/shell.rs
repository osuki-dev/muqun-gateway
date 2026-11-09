//! The window: sidebar, main pane, inspector and the title band in one
//! `SplitView`. The band is the session's header: its mark, title and state
//! on the leading side, its actions trailing, so nothing overlaps however
//! long the title gets.

use std::sync::Arc;

use rune::{col, el, row, text, Color, Cx, El, SurfaceFit};
use rune_kit::glyphs::TRIANGLE_DOWN;
use rune_kit::shell::{SidebarToggle, SplitView, Toolbar, INSPECTOR};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, IconButton, IconButtonVariant,
    Kit, Menu, MenuItem,
};

use super::copy::{tr, tr_ref};
use super::{
    art, composer, home, inbox, inspector, sessions, settings, tasks, tiles, timeline, title_of,
    Activity, OpenMenu,
};
use super::{Mode, UiState};
use crate::branding::{APP_MARK_PNG, APP_NAME};
use crate::model::commands::Command;
use crate::theme::look::Look;

pub fn view(s: &UiState, cx: &mut Cx<UiState>, k: &Kit, layer: &mut Vec<El>) -> El {
    let look = s.look();
    let page = match s.mode {
        // Composed for the card's size: the first frame guesses, the
        // layout read re-runs the view once the card is laid out.
        Mode::Home => home::view(
            s,
            k,
            cx.layout_size("main-pane").unwrap_or([1080.0, 860.0]),
            cx.layout_size("home-dock"),
            layer,
        ),
        Mode::Session if s.in_split() => tiles::view(s, k, layer),
        Mode::Session => session(s, k, &look, layer),
        Mode::Inbox => inbox::view(s, k),
        Mode::Tasks => tasks::view(s, k),
        Mode::Settings => settings::view(s, k, layer),
    };
    let mut main = col().key("main").w_full().h_full().min_h(0.0);
    if let Some(alert) = alert(s, k) {
        main = main.child(alert);
    }
    // Pages cross-fade and lift a little as the mode changes.
    let page = look.arrive(page.key(format!("page-{:?}", s.mode)), 0);
    let main = main.child(page.grow(1.0).min_h(0.0));
    // Home is full-bleed artwork; a toolbar strip would sit as a band over
    // the hero. Settings / hub live in the sidebar footer there.
    let frame = super::panes::FRAME;
    // The sidebar floats as a rounded panel inset from the window's edges,
    // a frame's width from the main card: its contents keep clear of the
    // panel's edges, its footer of the panel's foot.
    let sidebar = col()
        .key("sidebar-frame")
        .pb(frame)
        .child(sessions::view(s, k, layer).w_full().grow(1.0).min_h(0.0));
    let mut split = SplitView::new(main)
        .sidebar(&s.sidebar, super::sidebar_pane, sidebar)
        .sidebar_padding(frame, frame)
        .sidebar_leading(home_brand(
            s,
            k,
            &look,
            crate::platform::titlebar_inset(cx.titlebar_inset()),
        ));
    // Home has no toolbar of its own; its card still starts on the
    // sidebar's top row, as every page's does.
    // Settings, Inbox and Tasks have none either: their panes take the
    // whole card.
    split = split.toolbar(
        if matches!(
            s.mode,
            Mode::Home | Mode::Settings | Mode::Inbox | Mode::Tasks
        ) {
            Toolbar::new()
        } else {
            toolbar(s, k, &look, layer)
        },
    );
    let inspector_here = s.right_open && s.mode == Mode::Session && !s.in_split();
    // A row's details take the right-hand slot while they are open; an open
    // terminal takes it over both, and the inspector comes last.
    let details = (s.mode == Mode::Session)
        .then(|| timeline::detail_pane(s, k))
        .flatten();
    // The right-hand pane's width, for the main card's floor on a
    // see-through window.
    let mut right_w = 0.0;
    // Each right-hand pane floats in its own panel, as the sidebar does.
    let wash = if look.see_through {
        SEE_THROUGH_FLOOR[usize::from(look.dark)]
    } else {
        0.0
    };
    if s.term.mounted() && (s.term.open || (details.is_none() && !inspector_here)) {
        right_w = s.term.pane.shown_width();
        let scheme = if k.dark {
            crate::theme::color_scheme::ColorScheme::Dark
        } else {
            crate::theme::color_scheme::ColorScheme::Light
        };
        // The terminal's own background is its panel's floor, corner to
        // corner, a breath of the desktop through it as the card has.
        let alpha = if look.see_through {
            SEE_THROUGH_CARD[usize::from(look.dark)]
        } else {
            1.0
        };
        let surface = super::terminal_panel::surface(s, k, scheme).alpha(alpha);
        let view = super::terminal_panel::view(s, k, scheme, cx.env().scale, layer);
        split = split.inspector(
            &s.term.pane,
            super::terminal_pane,
            right_panel(k, wash, look.dark, Some(surface), view),
        );
    } else if let Some(pane) = details {
        right_w = s.inspect_pane.shown_width();
        split = split.inspector(
            &s.inspect_pane,
            super::inspect_pane,
            right_panel(k, wash, look.dark, None, pane),
        );
    } else if inspector_here {
        right_w = s.right.shown_width();
        split = split.inspector(
            &s.right,
            super::right_pane,
            right_panel(k, wash, look.dark, None, inspector::view(s, k)),
        );
    }
    let mut split = split
        .titlebar_inset(crate::platform::titlebar_inset(cx.titlebar_inset()))
        .frame_inset(frame)
        .pane_motion(super::panes::motion())
        .reduced_motion(!look.motion || cx.is_reduced_motion())
        .sidebar_toggle(
            SidebarToggle::new()
                .size(ControlSize::Large)
                .in_header()
                .unread(s.waiting_count() > 0)
                .shortcut(super::keymap::sidebar_label()),
        );
    split = split.toolbar_in_panel();
    // The window floor, the theme's world on it, the sidebar's panel, and
    // the chrome over them: the shell itself lays no floor of its own.
    let home = s.mode == Mode::Home;
    let mut chrome = k.clone();
    chrome.c.bg_deep = k.c.bg_deep.alpha(0.0);
    let window = cx.layout_size("shell");
    if look.see_through {
        // The desktop shows around the panels; each lays its own floor, the
        // main card with the theme's wallpaper in it.
        chrome.c.bg_base = k.c.bg_base.alpha(if home { 0.0 } else { CARD_ALPHA });
        let root = el()
            .key("window-stage")
            .w_full()
            .h_full()
            .child(sidebar_panel(
                s,
                k,
                SEE_THROUGH_FLOOR[usize::from(look.dark)],
            ))
            .children(card_floor(s, k, &look, window, right_w))
            .child(split.build(&chrome));
        return root.on_layout(|s: &mut UiState, r| s.fits.window = r[2]);
    }
    let mut stage = el().key("window-stage").w_full().h_full().fill(k.c.bg_deep);
    let mut panel_wash = 0.0;
    if let Some((backdrop, wash)) = backdrop(s, k, &look, window) {
        // The main card lets a breath of the wallpaper through (Home draws
        // its own stage over a clear card).
        chrome.c.bg_base = k.c.bg_base.alpha(if home { 0.0 } else { CARD_ALPHA });
        stage = stage.child(backdrop);
        if home {
            // Home keeps the wallpaper whole; the sidebar's lists read over
            // a calmer patch of it, inside their panel.
            panel_wash = wash;
        } else {
            stage = stage.child(
                el().key("window-wash")
                    .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                    .fill(k.c.bg_deep.alpha(wash))
                    .a11y_hidden(),
            );
        }
    }
    let root = stage
        .child(sidebar_panel(s, k, panel_wash))
        .child(split.build(&chrome));
    // The window's width bounds the right-hand panes (`UiState::fit_panes`).
    root.on_layout(|s: &mut UiState, r| s.fits.window = r[2])
}

/// The least of the window floor that returns under the sidebar's lists,
/// so a picture there is felt rather than read (light, dark): a picture
/// over a light floor reads louder.
const SIDEBAR_WASH: [f32; 2] = [0.62, 0.4];

/// The main card's opacity over the window's wallpaper outside Home.
const CARD_ALPHA: f32 = 0.9;

/// On a see-through window, how much of the window floor each panel lays
/// over the (compositor-blurred) desktop before its own surface (light,
/// dark): enough that its words read over any desktop.
const SEE_THROUGH_FLOOR: [f32; 2] = [0.82, 0.78];
/// The main card's floor there: denser, so the theme's world in it is the
/// theme's and not the desktop's.
const SEE_THROUGH_CARD: [f32; 2] = [0.93, 0.9];

/// The main card's floor on a see-through window: the window floor, and the
/// theme's wallpaper on it placed for the card, rounded as the card is.
/// Rides the panes' spring as the sidebar and the right-hand pane open.
fn card_floor(
    s: &UiState,
    k: &Kit,
    look: &Look,
    window: Option<[f32; 2]>,
    right_w: f32,
) -> Option<El> {
    let [w, h] = window?;
    let frame = super::panes::FRAME;
    let left = if s.sidebar.is_open() {
        s.sidebar.width()
    } else {
        frame
    };
    let right = if right_w > 0.0 { right_w } else { frame };
    let (cw, ch) = ((w - left - right).max(0.0), (h - frame * 2.0).max(0.0));
    let mut floor = el()
        .key("card-floor")
        .pin(Some(left), Some(frame), Some(right), Some(frame))
        .radius(k.radius.lg)
        .clip_rounded(k.radius.lg)
        .fill(k.c.bg_deep.alpha(SEE_THROUGH_CARD[usize::from(look.dark)]))
        .motion(super::panes::SPRING)
        .a11y_hidden();
    let wall = s.images.as_ref().and_then(|images| {
        let art = crate::theme::apply::backdrop(s.scheme(), s.mode == Mode::Home)?;
        (art.look.opacity > 0.01).then(|| {
            art::place(images, &art, "window-backdrop", cw, ch)
                .absolute(0.0, 0.0)
                .opacity(art.look.opacity)
                .opacity_motion(crate::theme::look::MOVE)
        })
    });
    match wall {
        Some(wall) => floor = floor.child(wall),
        // No wallpaper: the card is the page colour through and through.
        None => floor = floor.fill(k.c.bg_deep),
    }
    Some(floor)
}

/// How much of the page colour the sidebar's panel lays over the window
/// floor (light, dark): a half step between the floor and the main card,
/// so the card stays the one surface that leads.
const SIDEBAR_TINT: [f32; 2] = [0.62, 0.5];

/// The sidebar's panel: a rounded surface behind its contents, inset from
/// the window's edges and a frame's width from the main card, with the
/// card's radius and hairline. `wash` lays the floor back first, over a
/// wallpaper. It follows the pane open and shut on the panes' spring.
fn sidebar_panel(s: &UiState, k: &Kit, wash: f32) -> El {
    let frame = super::panes::FRAME;
    let open = s.sidebar.is_open();
    let motion = if s.sidebar.is_resizing() {
        rune::Spring::NONE
    } else {
        super::panes::SPRING
    };
    panel_surface(k, "sidebar-panel", wash, s.look().dark)
        .pin(Some(frame), Some(frame), None, Some(frame))
        .w((s.sidebar.width() - frame * 2.0).max(0.0))
        .opacity(if open { 1.0 } else { 0.0 })
        .opacity_motion(super::panes::SPRING)
        .motion(motion)
        .a11y_hidden()
}

/// A side panel's surface: the window floor at `wash`, the page colour over
/// it at the sidebar's tint, with the main card's radius and hairline.
fn panel_surface(k: &Kit, key: &str, wash: f32, dark: bool) -> El {
    let tint = SIDEBAR_TINT[usize::from(dark)];
    col()
        .key(key.to_string())
        .radius(k.radius.lg)
        .fill(k.c.bg_deep.alpha(wash))
        .border(1.0, k.c.border_muted)
        .clip_rounded(k.radius.lg)
        .child(
            el().key(format!("{key}-tint"))
                .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
                .fill(k.c.bg_base.alpha(tint))
                .a11y_hidden(),
        )
}

/// A right-hand pane in its own panel, as the sidebar's: a frame in from
/// the window's edges and from the main card (the window shows in the gap),
/// on its own floor (the sidebar's, or `surface` when the pane brings its
/// own), its content held inside the rounded corners. The slot already
/// starts a frame down, under the title strip.
fn right_panel(k: &Kit, wash: f32, dark: bool, surface: Option<Color>, content: El) -> El {
    let frame = super::panes::FRAME;
    let mut panel = panel_surface(k, "right-panel", wash, dark);
    if let Some(surface) = surface {
        panel = col()
            .key("right-panel")
            .radius(k.radius.lg)
            .fill(surface)
            .border(1.0, k.c.border_muted)
            .clip_rounded(k.radius.lg);
    }
    col()
        .key("right-frame")
        .w_full()
        .h_full()
        .pl(frame)
        .pr(frame)
        .pb(frame)
        .child(
            panel
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(content.w_full().h_full().min_h(0.0)),
        )
}

/// The theme's wallpaper across the whole window, behind sidebar and card:
/// `home.wallpaper` on Home, `shell.wallpaper` elsewhere, placed as the pack
/// asks at its opacity. With it, how much of the floor to lay back over the
/// sidebar so its text keeps its contrast over any picture. None under
/// reduce transparency: the chrome stays solid.
fn backdrop(s: &UiState, k: &Kit, look: &Look, window: Option<[f32; 2]>) -> Option<(El, f32)> {
    if !look.glass {
        return None;
    }
    let images = s.images.as_ref()?;
    let art = crate::theme::apply::backdrop(s.scheme(), s.mode == Mode::Home)?;
    let [w, h] = window?;
    let opacity = art.look.opacity;
    if opacity <= 0.01 {
        return None;
    }
    let wash = crate::theme::look::wash_for(
        k.c.bg_deep,
        &[(k.c.text_base, 4.5), (k.c.text_muted, 4.5)],
        opacity,
    )
    .max(SIDEBAR_WASH[usize::from(look.dark)]);
    Some((
        art::place(images, &art, "window-backdrop", w, h)
            .absolute(0.0, 0.0)
            .opacity(opacity)
            .opacity_motion(crate::theme::look::MOVE),
        wash,
    ))
}

/// Mark + name in the sidebar title bar; press returns to Home. The mark
/// carries a soft accent glow: the one lit thing in the chrome.
fn home_brand(s: &UiState, k: &Kit, look: &Look, lights: f32) -> El {
    let language = s.cfg.general.language;
    // Keep a fixed cross-size: the titlebar row stretches children to its full
    // height, and a stretched text run top-aligns while the mark stays centred.
    const MARK: f32 = 20.0;
    let mark = match s.images.as_ref() {
        Some(images) => art::picture(images, Arc::<[u8]>::from(APP_MARK_PNG), SurfaceFit::Fill)
            .key("home-mark")
            .w(MARK)
            .h(MARK)
            .radius(5.0)
            .clip_rounded(5.0),
        None => el()
            .key("home-mark")
            .w(MARK)
            .h(MARK)
            .radius(5.0)
            .fill(k.c.bg_accent),
    };
    let mark = look.glow(mark, k.c.bg_accent, 0.35);
    // With no window controls at the leading edge the mark lines up with
    // the rows' icons below it; past traffic lights it keeps a small gap.
    let lead = if lights > 0.0 { 6.0 } else { 16.0 };
    row()
        .key("home-brand")
        .ml(lead)
        .gap(8.0)
        .h(MARK)
        .center_y()
        .role("button")
        .label(tr_ref(language, "Home", "首页"))
        .child(mark)
        .child(
            text(APP_NAME)
                .key("home-name")
                .font(k.ty.ui_medium.lh(MARK))
                .color(if s.mode == Mode::Home {
                    k.c.text_base
                } else {
                    k.c.text_muted
                }),
        )
        .on_press(|s: &mut UiState| s.go(Mode::Home))
}

fn page_title(s: &UiState) -> String {
    let language = s.cfg.general.language;
    match s.mode {
        Mode::Home => tr(language, "New session", "新建会话"),
        Mode::Inbox => tr(language, "Inbox", "收件箱"),
        Mode::Tasks => tr(language, "Tasks", "任务"),
        Mode::Settings => tr(language, "Settings", "设置"),
        Mode::Session => s
            .current()
            .map_or_else(|| s.status.clone(), super::title_of),
    }
}

fn status_interesting(status: &str) -> bool {
    let s = status.trim();
    !(s.is_empty()
        || s == "Starting…"
        || s == "Sent"
        || s == "Demo"
        || s.starts_with("Opening ")
        || s.starts_with("Theme: ")
        || s.ends_with(" messages")
        || s.ends_with(" agent(s) discovered"))
}

/// The session's identity on the band's leading side: mark, title, agent
/// and model, branch, and what it is doing.
fn session_identity(s: &UiState, k: &Kit, look: &Look) -> El {
    let language = s.cfg.general.language;
    let mut line = row()
        .key("session-identity")
        .gap(10.0)
        .center_y()
        .shrink()
        .min_w(0.0);
    let Some(info) = s.current() else {
        return line.child(
            text(s.status.clone())
                .font(k.ty.ui_medium)
                .color(k.c.text_muted)
                .truncate(),
        );
    };
    let activity = s.activity(info);
    let mut meta = vec![s.agent_name(&info.agent_id)];
    if let Some(m) = &info.model {
        let name = super::labels::format_model_name(m, "", None);
        if !name.is_empty() {
            meta.push(name);
        }
    }
    if let Some(branch) = s.branch_of(info) {
        meta.push(branch);
    } else if let Some(folder) = UiState::folder_name(info) {
        meta.push(folder);
    }
    line = line
        .child(
            look.glow(
                el().key("session-mark")
                    .size(26.0, 26.0)
                    .radius(7.0)
                    .fill(super::agents::color(&info.agent_id, s.dark).alpha(0.14))
                    .center()
                    .child(super::agents::mark(&info.agent_id, 16.0, s.dark)),
                super::agents::color(&info.agent_id, s.dark),
                if activity == Activity::Working {
                    0.45
                } else {
                    0.0
                },
            )
            .a11y_hidden(),
        )
        .child(
            col()
                .key("session-words")
                .gap(0.0)
                .shrink()
                .min_w(0.0)
                .child(
                    text(title_of(info))
                        .key("session-title")
                        .font(k.ty.ui_medium.lh(17.0))
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(
                    text(meta.join(" · "))
                        .key("session-meta")
                        .font(k.ty.caption().lh(14.0))
                        .color(k.c.text_faint)
                        .truncate(),
                ),
        );
    if activity != Activity::Idle {
        let word = super::Fit::of("main") == super::Fit::Wide;
        line = line.child(super::activity_mark(k, look, activity, language, word));
    }
    line
}

/// Fork, Compact, Rename, Pin and Delete: the session header's overflow.
fn session_more(s: &UiState, k: &Kit, layer: &mut Vec<El>, asid: &str, busy: bool) -> El {
    let language = s.cfg.general.language;
    let open = s.menu == Some(OpenMenu::SessionMore);
    let (rename_id, delete_id, pin_id) = (asid.to_string(), asid.to_string(), asid.to_string());
    let (fork_id, compact_id) = (asid.to_string(), asid.to_string());
    let pinned = s.cfg.general.pinned.iter().any(|p| p == asid);
    Menu::new("session-more")
        .trigger(
            IconButton::new(&TRIANGLE_DOWN)
                .variant(IconButtonVariant::GhostMuted)
                .control_size(ControlSize::Small)
                .label(tr_ref(language, "More", "更多"))
                .expanded(open)
                .on_press(move |s: &mut UiState| {
                    s.menu = (!open).then_some(OpenMenu::SessionMore);
                })
                .build(k)
                .key("session-more-btn"),
        )
        .open(open)
        .on_close(|s: &mut UiState| s.menu = None)
        .item(
            MenuItem::new(tr(language, "Fork session", "分叉会话")).on_select(
                move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::ForkSession {
                        asid: muqun_gateway::agents::AgentSessionId(fork_id.clone()),
                        message_id: None,
                    });
                },
            ),
        )
        .item(
            MenuItem::new(tr(language, "Compact history", "压缩历史"))
                .disabled(busy)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::CompactSession {
                        asid: muqun_gateway::agents::AgentSessionId(compact_id.clone()),
                    });
                }),
        )
        .separator()
        .item(MenuItem::new(tr(language, "Rename…", "重命名…")).on_select(
            move |s: &mut UiState| {
                s.menu = None;
                let title = s
                    .session_by_id(&rename_id)
                    .map(title_of)
                    .unwrap_or_default();
                s.rename_asid = Some(rename_id.clone());
                s.rename_edit = rune::TextEdit::new(title);
                s.rename_edit.focused = true;
            },
        ))
        .item(
            MenuItem::new(if pinned {
                tr(language, "Unpin", "取消置顶")
            } else {
                tr(language, "Pin", "置顶")
            })
            .on_select(move |s: &mut UiState| {
                s.menu = None;
                s.edit(crate::config::ConfigEdit::TogglePin(pin_id.clone()));
            }),
        )
        .separator()
        .item(
            MenuItem::new(tr(language, "Delete session", "删除会话"))
                .danger(true)
                .on_select(move |s: &mut UiState| {
                    s.menu = None;
                    s.send(Command::DeleteSession(
                        muqun_gateway::agents::AgentSessionId(delete_id.clone()),
                    ));
                }),
        )
        .build(k, layer)
}

fn toolbar(s: &UiState, k: &Kit, look: &Look, layer: &mut Vec<El>) -> Toolbar {
    let language = s.cfg.general.language;
    let mut bar = Toolbar::new();
    match s.mode {
        Mode::Session if s.in_split() => {
            bar = bar.leading(tiles::toolbar_title(s, k));
            for action in tiles::toolbar_actions(s, k) {
                bar = bar.trailing(action);
            }
        }
        Mode::Session => {
            bar = bar.leading(session_identity(s, k, look));
            let asid = s.selected.clone().unwrap_or_default();
            let busy = s.busy();
            // A squeezed column keeps the title readable: Split folds away
            // (Ctrl+\\ still splits), then the change count.
            let fit = super::Fit::of("main");
            if let Some(meter) = timeline::context_meter(s, k, &asid) {
                bar = bar.trailing(meter);
            }
            if s.diff_stat.files > 0 && fit > super::Fit::Tight {
                let label = format!("+{} −{}", s.diff_stat.added, s.diff_stat.removed);
                bar = bar.trailing(
                    Button::new(label)
                        .variant(if s.show_diff {
                            ButtonVariant::Contrast
                        } else {
                            ButtonVariant::Ghost
                        })
                        .control_size(ControlSize::Small)
                        .help(format!(
                            "{} {}",
                            s.diff_stat.files,
                            tr_ref(language, "changed file(s)", "个文件有改动")
                        ))
                        .on_press(|s: &mut UiState| match s.selected.clone() {
                            Some(asid) => s.send(Command::OpenDiff {
                                asid: muqun_gateway::agents::AgentSessionId(asid),
                                file: None,
                            }),
                            None => s.show_diff = !s.show_diff,
                        })
                        .build(k)
                        .key("changes"),
                );
            }
            if fit == super::Fit::Wide {
                bar = bar.trailing(
                    Button::new(tr_ref(language, "Split view", "分屏"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .disabled(asid.is_empty())
                        .help(tr_ref(
                            language,
                            "Open another session beside this one",
                            "在旁边打开另一个会话",
                        ))
                        .shortcut(super::keymap::label(super::keymap::Action::AddSplit))
                        .on_press(|s: &mut UiState| s.split())
                        .build(k)
                        .key("split-session"),
                );
            }
            if !asid.is_empty() {
                bar = bar.trailing(session_more(s, k, layer, &asid, busy));
            }
        }
        _ => {
            bar = bar.title(page_title(s));
            if status_interesting(&s.status) {
                bar = bar.trailing(
                    Badge::new(s.status.clone())
                        .variant(BadgeVariant::Subtle)
                        .build(k)
                        .key("status"),
                );
            }
        }
    }
    if s.mode != Mode::Home {
        bar = bar.trailing(
            IconButton::new(&super::terminal_panel::TERMINAL_WINDOW)
                .variant(if s.term.open {
                    IconButtonVariant::Contrast
                } else {
                    IconButtonVariant::Ghost
                })
                .label(tr_ref(language, "Terminal", "终端"))
                .help(super::keymap::tip(
                    tr_ref(language, "Terminal", "终端"),
                    super::keymap::Action::ToggleTerminal,
                ))
                .on_press(|s: &mut UiState| {
                    if s.term.open {
                        s.close_terminal();
                    } else {
                        s.open_terminal();
                    }
                })
                .build(k)
                .key("terminal-toggle"),
        );
    }
    if s.mode == Mode::Session && !s.in_split() {
        bar = bar.trailing(
            IconButton::new(&INSPECTOR)
                .variant(if s.right_open && !s.term.open {
                    IconButtonVariant::Contrast
                } else {
                    IconButtonVariant::Ghost
                })
                .label(tr_ref(language, "Inspector", "检查器"))
                .help(super::keymap::tip(
                    tr_ref(language, "Inspector", "检查器"),
                    super::keymap::Action::ToggleInspector,
                ))
                .on_press(|s: &mut UiState| s.toggle_inspector())
                .build(k)
                .key("inspector-toggle"),
        );
    }
    bar
}

/// An error from the core: the message, what to check, and a way to dismiss it.
fn alert(s: &UiState, k: &Kit) -> Option<El> {
    let language = s.cfg.general.language;
    let err = s.error.as_ref()?;
    let mut card = Card::new()
        .accent(k.c.danger.icon)
        .title(err.message.clone(), None)
        .action(
            Button::new(tr_ref(language, "Dismiss", "关闭"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.error = None)
                .build(k)
                .key("dismiss-error"),
        );
    if let Some(detail) = &err.detail {
        card = card.description(detail.clone());
    }
    if err.unreachable {
        card = card.action(
            Button::new(tr_ref(language, "Retry", "重试"))
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| s.send(Command::Refresh))
                .build(k)
                .key("retry-error"),
        );
    }
    Some(
        col()
            .key("alert")
            .w_full()
            .px(16.0)
            .pt(8.0)
            .child(card.build(k)),
    )
}

/// A hairline of light under the title band, in the session agent's colour.
pub fn light_rule(k: &Kit, color: rune::Color, strength: f32) -> El {
    el().key("light-rule")
        .w_full()
        .h(1.0)
        .fill(k.c.border_muted)
        .linear_gradient(
            std::f32::consts::FRAC_PI_2,
            [
                (0.0, color.alpha(0.0)),
                (0.25, color.alpha(0.75 * strength)),
                (0.75, color.alpha(0.75 * strength)),
                (1.0, color.alpha(0.0)),
            ],
        )
        .a11y_hidden()
}

fn session(s: &UiState, k: &Kit, look: &Look, layer: &mut Vec<El>) -> El {
    let body = if s.show_diff {
        timeline::changes(s, k)
    } else {
        timeline::view(s, k)
    };
    let mut page = col().key("session").w_full().min_h(0.0);
    if let Some(info) = s.current() {
        let working = s.activity(info) == Activity::Working;
        let color = super::agents::color(&info.agent_id, s.dark);
        let rule = light_rule(k, color, if working { 1.0 } else { 0.45 });
        page = page.child(if working { look.pulse(rule) } else { rule });
    }
    if s.revert_staged {
        page = page.child(revert_banner(s, k));
    }
    page.child(body).child(composer::view(s, k, layer))
}

fn revert_banner(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    Card::new()
        .accent(k.c.warning.icon)
        .title(tr_ref(language, "Rollback staged", "已暂存回滚"), None)
        .description(tr(
            language,
            "Confirm to restore files to the marked message, or cancel.",
            "确认后会把文件恢复到标记的消息，或取消。",
        ))
        .action(
            Button::new(tr_ref(language, "Confirm", "确认"))
                .variant(ButtonVariant::Submit)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    if let Some(asid) = s.selected.clone() {
                        s.send(Command::CommitRevert {
                            asid: muqun_gateway::agents::AgentSessionId(asid),
                        });
                    }
                })
                .build(k)
                .key("revert-commit"),
        )
        .action(
            Button::new(tr_ref(language, "Cancel", "取消"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(|s: &mut UiState| {
                    if let Some(asid) = s.selected.clone() {
                        s.send(Command::ClearRevert {
                            asid: muqun_gateway::agents::AgentSessionId(asid),
                        });
                    }
                })
                .build(k)
                .key("revert-clear"),
        )
        .build(k)
        .key("revert-banner")
        .mx(16.0)
        .mt(8.0)
}
