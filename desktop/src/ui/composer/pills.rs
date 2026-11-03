//! The selector row under the field: 28 px mono pills -- project, agent,
//! model, mode or effort, branch -- each opening a menu, then the queue
//! count, the context readout and the primary action. A pill only appears
//! when there is something to choose: the model, mode and effort pills come
//! from the agent's catalogue and are left out when it has none.

use std::sync::Arc;

use blinc_cn::prelude::{icons, to_svg, ColorToken, ThemeState};
use blinc_layout::prelude::*;
use blinc_layout::widgets::overlay::AnchorDirection;
use blinc_layout::widgets::overlay_stack::{OverlayBuilder, OverlayHandle};
use muqun_gateway::agents::{ModelInfo, ModelRef};
use muqun_gateway::home_relative;

use super::state::{Cmp, MenuId};
use super::usage;
use super::{actions, Aim, Place};
use crate::model::commands::Command;
use crate::model::compose::ComposeCommand;
use crate::ui::agents::{self, Standing};
use crate::ui::text::{mono_text, ui_text};
use crate::ui::App;

const PILL_HEIGHT: f32 = 28.0;
/// Menu rows listed at most (a catalogue may hold hundreds of models).
const MAX_MODELS: usize = 14;
const MAX_PROJECTS: usize = 8;
const MAX_AGENTS: usize = 10;
/// Longest label a pill shows, in characters.
const LABEL_CHARS: usize = 22;

fn ico(path: &str, size: f32) -> impl ElementBuilder {
    svg(to_svg(path, size))
        .size(size, size)
        .tint(ThemeState::get().color(ColorToken::TextSecondary))
        .flex_shrink_0()
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max - 1).collect();
    format!("{cut}…")
}

/// `~/Work/muqun/gateway` → `muqun/gateway`; the home folder is `~`.
pub fn short_project(path: &str) -> String {
    let home = dirs::home_dir();
    let shown = home_relative(path, home.as_deref());
    if shown == "~" || shown == "/" {
        return shown;
    }
    let parts: Vec<&str> = shown.trim_end_matches('/').rsplit('/').take(2).collect();
    match parts.as_slice() {
        [name, parent] if !parent.is_empty() && *parent != "~" => format!("{parent}/{name}"),
        [name, ..] => (*name).to_string(),
        [] => shown,
    }
}

/// The folders to offer, newest first: the sessions', the ones agents last
/// started in, and the one showing.
pub fn recent_projects(app: &App, current: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |path: String| {
        if !path.is_empty() && !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(current) = current {
        add(current.to_string());
    }
    for session in app.bindings.sessions.get().iter() {
        if let Some(dir) = session.directory.clone() {
            add(dir);
        }
    }
    for path in app.config.get().general.last_cwd.values() {
        add(path.to_string_lossy().into_owned());
    }
    // The agent's own projects (a folder that is gone is not offered).
    let known = app.bindings.projects.get();
    for project in known.projects.iter().filter(|p| !p.missing) {
        add(project.canonical.clone());
    }
    out.truncate(MAX_PROJECTS);
    out
}

fn pill(icon: Option<Div>, label: String, chevron: bool, open: bool) -> Div {
    let mut pill = div()
        .class("mq-pill")
        .h(PILL_HEIGHT)
        .flex_row()
        .items_center();
    if chevron {
        pill = pill.cursor_pointer();
    }
    if open {
        pill = pill.class("mq-pill--open");
    }
    if let Some(icon) = icon {
        pill = pill.child(icon);
    }
    pill = pill.child(mono_text(label).size(12.0).no_wrap().no_cursor());
    if chevron {
        pill = pill.child(ico(icons::CHEVRON_DOWN, 12.0));
    }
    pill
}

fn glyph_box(path: &str) -> Div {
    div().flex_shrink_0().child(ico(path, 14.0))
}

/// Which side of its pill a menu opens on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Above,
    Below,
}

/// A menu's height: `rows` of 32 px, `separators` of 9 px, 10 px of padding
/// and border. Menus above their pill are placed by it (an overlay anchored
/// by its bottom edge uses a size it measured on an earlier frame).
fn menu_height(rows: usize, separators: usize) -> f32 {
    rows as f32 * 32.0 + separators as f32 * 9.0 + 10.0
}

/// Where a pill is, for placing its menu.
#[derive(Clone, Copy)]
struct Anchor {
    x: f32,
    y: f32,
    height: f32,
}

type Opener = Arc<dyn Fn(Anchor) + Send + Sync>;

thread_local! {
    /// Each pill's way to open its menu, by menu; the pill's click calls it,
    /// and so does the QA harness (`composer/qa.rs`).
    static OPENERS: std::cell::RefCell<std::collections::HashMap<MenuId, Opener>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// The element id of a menu's pill.
pub fn pill_id(id: MenuId) -> String {
    format!("mq-pill-{}", format!("{id:?}").to_lowercase())
}

/// Open `id`'s menu at its pill, as a click would (QA).
pub fn open_by_id(id: MenuId) -> bool {
    let Some(bounds) = blinc_layout::selector::query(&pill_id(id)).and_then(|h| h.bounds()) else {
        return false;
    };
    let Some(open) = OPENERS.with(|o| o.borrow().get(&id).cloned()) else {
        return false;
    };
    open(Anchor {
        x: bounds.x,
        y: bounds.y,
        height: bounds.height,
    });
    true
}

/// A pill that opens `menu` in the overlay layer -- above every sibling, so
/// a menu hanging over the start page's recents is not drawn through by
/// their text. A press outside the menu closes it; so does Esc.
fn host(
    id: MenuId,
    side: Side,
    pill: Div,
    size: (f32, f32),
    menu: impl Fn() -> Div + Send + Sync + 'static,
) -> Div {
    let menu = Arc::new(menu);
    let open: Opener = Arc::new(move |at: Anchor| {
        let cmp = Cmp::get();
        // One menu at a time.
        close_menu();
        let menu = menu.clone();
        let (x, y, anchor) = match side {
            Side::Below => (at.x, at.y + at.height + 4.0, AnchorDirection::Bottom),
            Side::Above => (at.x, at.y - 4.0 - size.1, AnchorDirection::Bottom),
        };
        let closed = cmp.clone();
        let handle = OverlayBuilder::popover()
            .at(x, y)
            .size(size.0, size.1)
            .anchor_direction(anchor)
            .on_close(move |_| {
                closed.menu.signal().set(None);
                closed.menu_handle.signal().set(None);
                blinc_layout::click_outside::unregister_click_outside("mq-cmp-menu");
            })
            .content(move || {
                div()
                    .class("mq-menu")
                    .id("mq-cmp-menu-panel")
                    .flex_col()
                    .child(menu())
            })
            .show();
        let raw = handle.raw();
        blinc_layout::click_outside::register_click_outside(
            "mq-cmp-menu",
            "mq-cmp-menu-panel",
            move || {
                super::state::mark_menu_closed(id);
                OverlayHandle::from_raw(raw).close();
            },
        );
        cmp.menu_handle.signal().set(Some(raw));
        cmp.menu.signal().set(Some(id));
    });
    OPENERS.with(|o| o.borrow_mut().insert(id, open.clone()));
    let trigger = pill.id(pill_id(id)).on_click(move |ctx| {
        if super::state::menu_just_closed(id) {
            return;
        }
        if Cmp::get().menu.get() == Some(id) {
            close_menu();
            return;
        }
        open(Anchor {
            x: ctx.bounds_x,
            y: ctx.bounds_y,
            height: ctx.bounds_height,
        });
    });
    div().flex_shrink_0().child(trigger)
}

/// Close the open menu, if any (Esc, a row taken, another menu opening).
pub fn close_menu() {
    let cmp = Cmp::get();
    if let Some(raw) = cmp.menu_handle.get() {
        let handle = OverlayHandle::from_raw(raw);
        if handle.is_live() {
            handle.close();
            return;
        }
    }
    cmp.menu.signal().set(None);
    cmp.menu_handle.signal().set(None);
}

fn menu_row(label: Div, selected: bool, enabled: bool) -> Div {
    let mut row = div()
        .class("mq-menu-row")
        .h(32.0)
        .w_full()
        .flex_row()
        .items_center()
        .child(label.flex_1().min_w(0.0));
    if selected {
        row = row.class("mq-menu-row--sel").child(ico(icons::CHECK, 14.0));
    }
    if enabled {
        row.cursor_pointer()
    } else {
        row.class("mq-menu-row--off")
    }
}

fn separator() -> Div {
    div().class("mq-menu-sep").w_full().h(1.0)
}

/// The whole selector row.
pub fn row(app: &App, aim: &Aim) -> Div {
    let cmp = Cmp::get();
    let start = aim.place == Place::Start;
    let side = if start { Side::Below } else { Side::Above };
    let mut pills = div()
        .class("mq-pills")
        .flex_1()
        .min_w(0.0)
        .flex_row()
        .flex_wrap()
        .items_center();
    pills = pills.child(project_pill(app, aim, side));
    pills = pills.child(agent_pill(app, aim, side));
    if let Some(catalog) = aim.catalog.as_ref() {
        let models: Vec<&ModelInfo> = catalog
            .catalog
            .models
            .iter()
            .filter(|m| m.enabled)
            .collect();
        if !models.is_empty() {
            pills = pills.child(model_pill(app, aim, &models, side));
        }
        let modes = visible_modes(aim);
        if !modes.is_empty() {
            pills = pills.child(mode_pill(app, aim, side));
        }
        if let Some(model) = selected_model(aim, &models) {
            if model.variants.as_ref().is_some_and(|v| !v.is_empty()) {
                pills = pills.child(effort_pill(app, aim, model, side));
            }
        }
    }
    if let Some(pill) = branch_pill(app, aim, side) {
        pills = pills.child(pill);
    }

    let mut right = div()
        .class("mq-cmp-right")
        .flex_row()
        .items_center()
        .flex_shrink_0();
    if aim.place == Place::Session {
        let queued = cmp.inbox.get().len();
        if queued > 0 {
            right = right.child(
                mono_text(app.strings.n_queued(queued))
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            );
        }
        if let Some(pill) = usage_readout(aim) {
            right = right.child(pill);
        }
    }
    right = right.child(actions(app, aim));

    div()
        .class("mq-cmp-pill-row")
        .w_full()
        .flex_row()
        .items_start()
        .child(pills)
        .child(right)
}

fn project_pill(app: &App, aim: &Aim, side: Side) -> Div {
    let label = aim
        .cwd
        .as_deref()
        .map(short_project)
        .unwrap_or_else(|| app.strings.no_folder().to_string());
    let label = clip(&label, LABEL_CHARS);
    if aim.place == Place::Session {
        // A session lives in its folder.
        return pill(Some(glyph_box(icons::FOLDER)), label, false, false).class("mq-pill--static");
    }
    let open = Cmp::get().menu.get() == Some(MenuId::Project);
    let p = pill(Some(glyph_box(icons::FOLDER)), label, true, open);
    let current = aim.cwd.clone();
    let app = app.clone();
    let height = menu_height(recent_projects(&app, current.as_deref()).len() + 1, 1);
    host(MenuId::Project, side, p, (320.0, height), move || {
        let mut menu = div().flex_col().min_w(300.0);
        for path in recent_projects(&app, current.as_deref()) {
            let shown = clip(&home_relative(&path, dirs::home_dir().as_deref()), 40);
            let selected = current.as_deref() == Some(path.as_str());
            let name = short_project(&path);
            let label = div()
                .flex_row()
                .items_center()
                .child(ui_text(name).size(13.0).no_wrap().no_cursor())
                .child(
                    div()
                        .class("mq-menu-dim")
                        .child(mono_text(shown).size(11.0).no_wrap().no_cursor()),
                );
            menu = menu.child(menu_row(label, selected, true).on_click(move |_| {
                super::set_project(path.clone());
                close_menu();
            }));
        }
        let browse = app.clone();
        menu.child(separator()).child(
            menu_row(
                div().child(
                    ui_text(app.strings.browse())
                        .size(13.0)
                        .no_wrap()
                        .no_cursor(),
                ),
                false,
                true,
            )
            .on_click(move |_| {
                close_menu();
                super::browse_project(&browse);
            }),
        )
    })
}

fn agent_pill(app: &App, aim: &Aim, side: Side) -> Div {
    let usable = aim.standing == Standing::Usable;
    let mark = if usable {
        agents::mark(&aim.agent, 14.0)
    } else {
        super::mark_idle(&aim.agent)
    };
    let label = clip(&aim.agent_name, LABEL_CHARS);
    if aim.place == Place::Session {
        return pill(Some(div().flex_shrink_0().child(mark)), label, false, false)
            .class("mq-pill--static");
    }
    let open = Cmp::get().menu.get() == Some(MenuId::Agent);
    let p = pill(Some(div().flex_shrink_0().child(mark)), label, true, open);
    let app = app.clone();
    let current = aim.agent.clone();
    let height = menu_height(app.bindings.agents.get().len().min(MAX_AGENTS), 0);
    host(MenuId::Agent, side, p, (340.0, height), move || {
        let list = app.bindings.agents.get();
        let mut menu = div().flex_col().min_w(320.0);
        for (info, standing) in agents::by_standing(&list).into_iter().take(MAX_AGENTS) {
            let name = agents::name(&info.id, &list);
            let ok = standing == Standing::Usable;
            let mut label = div()
                .flex_row()
                .items_center()
                .child(if ok {
                    agents::mark(&info.id, 16.0)
                } else {
                    agents::mark_idle(&info.id, 16.0)
                })
                .child(
                    div()
                        .class("mq-menu-gap")
                        .child(ui_text(name).size(13.0).no_wrap().no_cursor()),
                );
            let mut word = standing.word(&app.strings).to_string();
            if !ok {
                if let Some(reason) = info.reason.clone().filter(|r| !r.is_empty()) {
                    word = format!("{word} — {}", clip(&reason, 28));
                }
            }
            label = label.child(
                div()
                    .class("mq-menu-dim")
                    .flex_1()
                    .child(ui_text(word).size(11.0).no_wrap().no_cursor()),
            );
            let mut row = menu_row(label, ok && info.id == current, ok);
            if ok {
                let id = info.id.clone();
                row = row.on_click(move |_| {
                    let cmp = Cmp::get();
                    let mut choice = (*cmp.start.get()).clone();
                    choice.agent = Some(id.clone());
                    choice.model = None;
                    choice.mode = None;
                    cmp.start.signal().set(Arc::new(choice));
                    close_menu();
                });
            } else {
                // The way to fix it, beside the reason.
                let go = app.clone();
                row = row.child(
                    div()
                        .class("mq-menu-link")
                        .cursor_pointer()
                        .on_click(move |_| {
                            close_menu();
                            go.open_settings();
                        })
                        .child(
                            ui_text(app.strings.settings())
                                .size(11.0)
                                .no_wrap()
                                .no_cursor(),
                        ),
                );
            }
            menu = menu.child(row);
        }
        menu
    })
}

fn visible_modes(aim: &Aim) -> Vec<(String, String)> {
    aim.catalog
        .as_ref()
        .map(|c| {
            c.catalog
                .modes
                .iter()
                .filter(|m| !m.hidden)
                .map(|m| (m.id.clone(), mode_label(&m.id, &m.name)))
                .collect()
        })
        .unwrap_or_default()
}

/// The catalogue's name, else the id made readable
/// (`bypassPermissions` → `Bypass permissions`).
pub fn mode_label(id: &str, name: &str) -> String {
    if !name.trim().is_empty() {
        return name.to_string();
    }
    let mut out = String::new();
    for (i, c) in id.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else if i == 0 {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out.replace(['-', '_'], " ")
}

fn selected_model<'a>(aim: &Aim, models: &[&'a ModelInfo]) -> Option<&'a ModelInfo> {
    let chosen = aim.model.as_ref()?;
    models
        .iter()
        .find(|m| m.id == chosen.model_id && m.provider_id == chosen.provider_id)
        .copied()
}

fn model_pill(app: &App, aim: &Aim, models: &[&ModelInfo], side: Side) -> Div {
    let label = match (selected_model(aim, models), &aim.model) {
        (Some(model), _) => model.name.clone(),
        (None, Some(chosen)) => chosen.model_id.clone(),
        (None, None) => app.strings.model_default().to_string(),
    };
    let open = Cmp::get().menu.get() == Some(MenuId::Model);
    let p = pill(None, clip(&label, LABEL_CHARS), true, open);
    let mut list: Vec<ModelInfo> = models.iter().map(|m| (*m).clone()).collect();
    list.sort_by_key(|m| m.name.to_lowercase());
    let chosen = aim.model.clone();
    let asid = aim.asid();
    let app = app.clone();
    let height = menu_height(list.len().min(MAX_MODELS), 0)
        + if list.len() > MAX_MODELS { 18.0 } else { 0.0 };
    host(MenuId::Model, side, p, (300.0, height), move || {
        let mut menu = div().flex_col().min_w(260.0);
        let total = list.len();
        for model in list.iter().take(MAX_MODELS).cloned() {
            let selected = chosen
                .as_ref()
                .is_some_and(|c| c.model_id == model.id && c.provider_id == model.provider_id);
            let label = div()
                .flex_row()
                .items_center()
                .child(ui_text(model.name.clone()).size(13.0).no_wrap().no_cursor())
                .child(
                    div().class("mq-menu-dim").child(
                        mono_text(model.provider_id.clone())
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                );
            let app = app.clone();
            let asid = asid.clone();
            menu = menu.child(menu_row(label, selected, true).on_click(move |_| {
                let model = ModelRef {
                    provider_id: model.provider_id.clone(),
                    model_id: model.id.clone(),
                    variant: None,
                };
                choose_model(&app, asid.as_ref(), model);
                close_menu();
            }));
        }
        if total > MAX_MODELS {
            menu = menu.child(
                div().class("mq-menu-dim").child(
                    ui_text(format!("+{}", total - MAX_MODELS))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        menu
    })
}

/// The model (and its effort) the session or the start page will use.
fn choose_model(app: &App, asid: Option<&muqun_gateway::agents::AgentSessionId>, model: ModelRef) {
    match asid {
        Some(asid) => app.send(Command::Compose(ComposeCommand::SwitchModel {
            asid: asid.clone(),
            model,
        })),
        None => {
            let cmp = Cmp::get();
            let mut choice = (*cmp.start.get()).clone();
            choice.model = Some(model);
            cmp.start.signal().set(Arc::new(choice));
        }
    }
}

fn mode_pill(app: &App, aim: &Aim, side: Side) -> Div {
    let modes = visible_modes(aim);
    let label = aim
        .mode
        .as_deref()
        .map(|id| {
            modes
                .iter()
                .find(|(m, _)| m == id)
                .map(|(_, l)| l.clone())
                .unwrap_or_else(|| mode_label(id, ""))
        })
        .unwrap_or_else(|| app.strings.mode_word().to_string());
    let open = Cmp::get().menu.get() == Some(MenuId::Mode);
    let p = pill(
        Some(glyph_box(icons::FILE_TEXT)),
        clip(&label, LABEL_CHARS),
        true,
        open,
    );
    let chosen = aim.mode.clone();
    let asid = aim.asid();
    let app = app.clone();
    let height = menu_height(modes.len(), 0);
    host(MenuId::Mode, side, p, (220.0, height), move || {
        let mut menu = div().flex_col().min_w(200.0);
        for (id, label) in modes.clone() {
            let selected = chosen.as_deref() == Some(id.as_str());
            let app = app.clone();
            let asid = asid.clone();
            menu = menu.child(
                menu_row(
                    div().child(ui_text(label).size(13.0).no_wrap().no_cursor()),
                    selected,
                    true,
                )
                .on_click(move |_| {
                    match &asid {
                        Some(asid) => app.send(Command::Compose(ComposeCommand::SwitchMode {
                            asid: asid.clone(),
                            mode: id.clone(),
                        })),
                        None => {
                            let cmp = Cmp::get();
                            let mut choice = (*cmp.start.get()).clone();
                            choice.mode = Some(id.clone());
                            cmp.start.signal().set(Arc::new(choice));
                        }
                    }
                    close_menu();
                }),
            );
        }
        menu
    })
}

fn effort_pill(app: &App, aim: &Aim, model: &ModelInfo, side: Side) -> Div {
    let variants: Vec<String> = model
        .variants
        .as_ref()
        .map(|v| v.iter().map(|v| v.id.clone()).collect())
        .unwrap_or_default();
    let current = aim.model.as_ref().and_then(|m| m.variant.clone());
    let label = current
        .clone()
        .unwrap_or_else(|| app.strings.effort_default().to_string());
    let open = Cmp::get().menu.get() == Some(MenuId::Effort);
    let p = pill(None, clip(&label, LABEL_CHARS), true, open);
    let base = ModelRef {
        provider_id: model.provider_id.clone(),
        model_id: model.id.clone(),
        variant: None,
    };
    let asid = aim.asid();
    let app = app.clone();
    let height = menu_height(variants.len(), 0);
    host(MenuId::Effort, side, p, (180.0, height), move || {
        let mut menu = div().flex_col().min_w(160.0);
        for variant in variants.clone() {
            let selected = current.as_deref() == Some(variant.as_str());
            let app = app.clone();
            let asid = asid.clone();
            let base = base.clone();
            menu = menu.child(
                menu_row(
                    div().child(mono_text(variant.clone()).size(12.0).no_wrap().no_cursor()),
                    selected,
                    true,
                )
                .on_click(move |_| {
                    choose_model(
                        &app,
                        asid.as_ref(),
                        ModelRef {
                            variant: Some(variant.clone()),
                            ..base.clone()
                        },
                    );
                    close_menu();
                }),
            );
        }
        menu
    })
}

fn branch_pill(app: &App, aim: &Aim, side: Side) -> Option<Div> {
    let cwd = aim.cwd.clone()?;
    let branch = Cmp::get().branches.get().get(&cwd).cloned().flatten()?;
    let open = Cmp::get().menu.get() == Some(MenuId::Branch);
    let p = pill(
        Some(glyph_box(icons::GIT_BRANCH)),
        clip(&branch, LABEL_CHARS),
        true,
        open,
    );
    let app = app.clone();
    Some(host(
        MenuId::Branch,
        side,
        p,
        (280.0, menu_height(2, 1)),
        move || {
            let current = div()
                .flex_row()
                .items_center()
                .child(mono_text(branch.clone()).size(12.0).no_wrap().no_cursor())
                .child(
                    div().class("mq-menu-dim").child(
                        ui_text(app.strings.current_branch())
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                );
            // Switching a checkout the agent is using is not offered; a new
            // worktree is the way to branch off, and waits on the gateway's
            // worktree API.
            div()
                .flex_col()
                .min_w(260.0)
                .child(menu_row(current, true, false))
                .child(separator())
                .child(menu_row(
                    div()
                        .flex_row()
                        .items_center()
                        .child(
                            ui_text(app.strings.new_worktree())
                                .size(13.0)
                                .no_wrap()
                                .no_cursor(),
                        )
                        .child(
                            div().class("mq-menu-dim").child(
                                ui_text(app.strings.soon()).size(11.0).no_wrap().no_cursor(),
                            ),
                        ),
                    false,
                    false,
                ))
        },
    ))
}

/// "12.4k / 200k · $0.42", with the breakdown in a tooltip on click.
fn usage_readout(aim: &Aim) -> Option<Div> {
    let info = aim.info.as_ref()?;
    let catalog_limit = aim.catalog.as_ref().and_then(|c| {
        let chosen = aim.model.as_ref()?;
        c.catalog
            .models
            .iter()
            .find(|m| m.id == chosen.model_id && m.provider_id == chosen.provider_id)?
            .limit
            .as_ref()?
            .get("context")?
            .as_u64()
    });
    let usage = usage::summary(info, catalog_limit)?;
    let open = Cmp::get().menu.get() == Some(MenuId::Usage);
    let readout = div()
        .class("mq-usage")
        .h(PILL_HEIGHT)
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(
            mono_text(usage.label.clone())
                .size(11.0)
                .no_wrap()
                .no_cursor(),
        );
    let lines = usage.lines.clone();
    Some(host(
        MenuId::Usage,
        Side::Above,
        readout,
        (220.0, lines.len() as f32 * 15.0 + 14.0),
        move || {
            let mut tip = div().flex_col().min_w(180.0).class("mq-usage-tip");
            for line in lines.iter().cloned() {
                tip = tip.child(mono_text(line).size(11.0).no_wrap().no_cursor());
            }
            tip
        },
    ))
    .map(|h| if open { h.class("mq-usage--open") } else { h })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_are_named_by_the_catalogue_else_made_readable() {
        assert_eq!(mode_label("plan", "Plan"), "Plan");
        assert_eq!(mode_label("bypassPermissions", ""), "Bypass permissions");
        assert_eq!(mode_label("accept-edits", ""), "Accept edits");
    }

    #[test]
    fn a_project_shows_its_parent_and_name() {
        assert_eq!(short_project("/srv/work/gateway"), "work/gateway");
        assert_eq!(short_project("/gateway"), "gateway");
        assert_eq!(clip("abcdefghij", 5), "abcd…");
    }
}
