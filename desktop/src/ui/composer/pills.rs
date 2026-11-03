//! The selector row under the field: 28 px mono pills -- project, agent,
//! model, mode or effort, branch -- each opening a menu, then the queue
//! count, the context readout and the primary action. A pill only appears
//! when there is something to choose: the model, mode and effort pills come
//! from the agent's catalogue and are left out when it has none.
//!
//! Open menus answer ↑ / ↓ / Enter (see [`on_menu_key`]).

use std::cell::RefCell;
use std::sync::Arc;

use blinc_cn::prelude::{input, InputSize};
use blinc_layout::prelude::*;
use blinc_layout::widgets::overlay::AnchorDirection;
use blinc_layout::widgets::overlay_stack::{OverlayBuilder, OverlayHandle};
use muqun_gateway::agents::{ModelInfo, ModelRef};
use muqun_gateway::home_relative;

use super::state::{self, Cmp, MenuId};
use super::usage;
use super::{actions, Aim, Place};
use crate::model::commands::Command;
use crate::model::compose::ComposeCommand;
use crate::ui::agents::{self, Standing};
use crate::ui::glyphs::Glyph;
use crate::ui::icons;
use crate::ui::text::{mono_text, ui_text};
use crate::ui::App;

const KEY_UP: u32 = 38;
const KEY_DOWN: u32 = 40;
const KEY_ENTER: u32 = 13;

type MenuAction = Arc<dyn Fn() + Send + Sync + 'static>;

const PILL_HEIGHT: f32 = 28.0;
/// Menu rows listed at most (a catalogue may hold hundreds of models).
const MAX_MODELS: usize = 40;
const MAX_BRANCHES: usize = 24;
const MAX_PROJECTS: usize = 8;
const MAX_AGENTS: usize = 10;
/// Show a search field once the catalogue has more than this many models.
const MODEL_SEARCH_AFTER: usize = 8;
/// Search field height inside a menu (padding included).
const MENU_SEARCH_HEIGHT: f32 = 36.0;
/// Longest label a pill shows, in characters.
const LABEL_CHARS: usize = 22;

fn ico(glyph: Glyph, size: f32) -> impl ElementBuilder {
    icons::tinted(glyph, size)
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
        pill = pill.child(ico(Glyph::DisclosureDown, 12.0));
    }
    pill
}

fn glyph_box(glyph: Glyph) -> Div {
    div().flex_shrink_0().child(ico(glyph, 14.0))
}

/// Which side of its pill a menu opens on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Above,
    #[allow(dead_code)]
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
    static OPENERS: RefCell<std::collections::HashMap<MenuId, Opener>> =
        RefCell::new(std::collections::HashMap::new());
    /// Enabled rows of the open menu, in order, for ↑ / ↓ / Enter.
    static MENU_ACTIONS: RefCell<Vec<MenuAction>> = RefCell::new(Vec::new());
}

fn clear_menu_actions() {
    MENU_ACTIONS.with(|a| a.borrow_mut().clear());
}

fn push_menu_action(action: MenuAction) -> usize {
    MENU_ACTIONS.with(|a| {
        let i = a.borrow().len();
        a.borrow_mut().push(action);
        i
    })
}

/// Whether a pill menu is open.
pub fn menu_open() -> bool {
    Cmp::get().menu.get().is_some()
}

/// ↑ / ↓ move the focus; Enter takes the focused row. True when handled.
pub fn on_menu_key(key: u32) -> bool {
    if !menu_open() {
        return false;
    }
    let n = MENU_ACTIONS.with(|a| a.borrow().len());
    if n == 0 {
        return key == KEY_UP || key == KEY_DOWN || key == KEY_ENTER;
    }
    let cmp = Cmp::get();
    match key {
        KEY_UP => {
            let cur = cmp.menu_cursor.get();
            cmp.menu_cursor.signal().set(cur.saturating_sub(1));
            true
        }
        KEY_DOWN => {
            let cur = cmp.menu_cursor.get();
            cmp.menu_cursor.signal().set((cur + 1).min(n - 1));
            true
        }
        KEY_ENTER => {
            let cur = cmp.menu_cursor.get().min(n - 1);
            let action = MENU_ACTIONS.with(|a| a.borrow().get(cur).cloned());
            if let Some(action) = action {
                action();
            }
            true
        }
        _ => false,
    }
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
        cmp.menu_cursor.signal().set(0);
        let handle = OverlayBuilder::popover()
            .at(x, y)
            .size(size.0, size.1)
            .anchor_direction(anchor)
            .on_close(move |_| {
                closed.menu.signal().set(None);
                closed.menu_handle.signal().set(None);
                clear_menu_actions();
                state::clear_menu_query();
                blinc_layout::click_outside::unregister_click_outside("mq-cmp-menu");
            })
            .content(move || {
                let cmp = Cmp::get();
                let cursor = cmp.menu_cursor;
                let query = cmp.menu_query;
                let menu = menu.clone();
                div()
                    .class("mq-menu")
                    .id("mq-cmp-menu-panel")
                    .flex_col()
                    .child(
                        stateful_with_key::<NoState>("mq-cmp-menu-body")
                            .deps([cursor.signal_id(), query.signal_id()])
                            .on_state(move |_| {
                                clear_menu_actions();
                                let body = menu();
                                let n = MENU_ACTIONS.with(|a| a.borrow().len());
                                if n > 0 {
                                    let cur = Cmp::get().menu_cursor.get();
                                    if cur >= n {
                                        Cmp::get().menu_cursor.signal().set(n - 1);
                                    }
                                }
                                body
                            }),
                    )
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
        state::clear_menu_query();
        cmp.menu_cursor.signal().set(0);
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
    clear_menu_actions();
    state::clear_menu_query();
}

fn menu_row(label: Div, selected: bool, enabled: bool, focused: bool) -> Div {
    let mut row = div()
        .class("mq-menu-row")
        .h(32.0)
        .w_full()
        .flex_row()
        .items_center()
        .child(label.flex_1().min_w(0.0));
    if focused {
        row = row.class("mq-menu-row--focus");
    }
    if selected {
        row = row.class("mq-menu-row--sel").child(ico(Glyph::Done, 14.0));
    }
    if enabled {
        row.cursor_pointer()
    } else {
        row.class("mq-menu-row--off")
    }
}

/// An enabled row that Enter / click can take; registers for keyboard nav.
fn action_row(label: Div, selected: bool, action: impl Fn() + Send + Sync + 'static) -> Div {
    let action: MenuAction = Arc::new(action);
    let index = push_menu_action(action.clone());
    let focused = Cmp::get().menu_cursor.get() == index;
    menu_row(label, selected, true, focused).on_click(move |_| action())
}

fn separator() -> Div {
    div().class("mq-menu-sep").w_full().h(1.0)
}

/// The whole selector row.
pub fn row(app: &App, aim: &Aim) -> Div {
    let cmp = Cmp::get();
    // Composer sits at the foot on both pages; menus open upward.
    let side = Side::Above;
    let mut pills = div()
        .class("mq-pills")
        .flex_1()
        .min_w(0.0)
        .flex_row()
        .flex_wrap()
        .items_center();
    pills = pills.child(project_pill(app, aim, side));
    pills = pills.child(agent_pill(app, aim, side));
    let models: Vec<&ModelInfo> = aim
        .catalog
        .as_ref()
        .map(|c| c.catalog.models.iter().filter(|m| m.enabled).collect())
        .unwrap_or_default();
    if !models.is_empty() || aim.model.is_some() {
        pills = pills.child(model_pill(app, aim, &models, side));
    }
    // Mode is catalogue-backed: hide when the agent has nothing to choose.
    let modes = visible_modes(aim);
    if !modes.is_empty() {
        pills = pills.child(mode_pill(app, aim, side));
    }
    if let Some(model) = selected_model(aim, &models) {
        if model.variants.as_ref().is_some_and(|v| !v.is_empty()) {
            pills = pills.child(effort_pill(app, aim, model, side));
        }
    }
    // Branch name lives in the right git column; a compact control opens it.
    pills = pills.child(git_toggle(app, aim));

    let mut right = div()
        .class("mq-cmp-right")
        .flex_row()
        .items_center()
        .flex_shrink_0();
    if aim.place == Place::Session {
        let queued = cmp.inbox.get().len();
        if queued > 0 {
            let open_q = app.clone();
            right = right.child(
                div()
                    .class("mq-queued-jump")
                    .cursor_pointer()
                    .child(
                        mono_text(app.strings.n_queued(queued))
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    )
                    .on_click(move |_| {
                        crate::ui::shell::open_right(
                            &open_q,
                            crate::model::view::RightTab::Queue,
                        );
                    }),
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
        return pill(Some(glyph_box(Glyph::Folder)), label, false, false).class("mq-pill--static");
    }
    let open = Cmp::get().menu.get() == Some(MenuId::Project);
    let p = pill(Some(glyph_box(Glyph::Folder)), label, true, open);
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
            menu = menu.child(action_row(label, selected, move || {
                super::set_project(path.clone());
                close_menu();
            }));
        }
        let browse = app.clone();
        menu.child(separator()).child(action_row(
            div().child(
                ui_text(app.strings.browse())
                    .size(13.0)
                    .no_wrap()
                    .no_cursor(),
            ),
            false,
            move || {
                close_menu();
                super::browse_project(&browse);
            },
        ))
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
            let row = if ok {
                let id = info.id.clone();
                action_row(label, info.id == current, move || {
                    let cmp = Cmp::get();
                    let mut choice = (*cmp.start.get()).clone();
                    choice.agent = Some(id.clone());
                    choice.model = None;
                    choice.mode = None;
                    cmp.start.signal().set(Arc::new(choice));
                    close_menu();
                })
            } else {
                // The way to fix it, beside the reason.
                let go = app.clone();
                menu_row(label, false, false, false).child(
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
                )
            };
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

fn model_matches(model: &ModelInfo, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    model.name.to_lowercase().contains(&q)
        || model.id.to_lowercase().contains(&q)
        || model.provider_id.to_lowercase().contains(&q)
}

fn model_search_field(app: &App) -> Div {
    let filter = Cmp::get().model_filter.clone();
    let placeholder = app.strings.model_search().to_string();
    if let Ok(mut data) = filter.lock() {
        data.placeholder = placeholder;
    }
    div()
        .class("mq-menu-search")
        .w_full()
        .h(MENU_SEARCH_HEIGHT)
        .flex_row()
        .items_center()
        .child(
            div().flex_1().min_w(0.0).child(
                input(&filter)
                    .size(InputSize::Small)
                    .w_full()
                    .border_width(0.0)
                    .border_colors(
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                    )
                    .bg_colors(
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                    )
                    .on_change(move |text: &str| {
                        let next = text.trim().to_lowercase();
                        let cmp = Cmp::get();
                        if cmp.menu_query.get() != next {
                            cmp.menu_query.signal().set(next);
                            cmp.menu_cursor.signal().set(0);
                        }
                    }),
            ),
        )
}

fn model_pill(app: &App, aim: &Aim, models: &[&ModelInfo], side: Side) -> Div {
    let label = match (selected_model(aim, models), &aim.model) {
        (Some(model), _) => model.name.clone(),
        (None, Some(chosen)) => chosen.model_id.clone(),
        (None, None) => app.strings.model_default().to_string(),
    };
    let choosable = !models.is_empty();
    let open = Cmp::get().menu.get() == Some(MenuId::Model);
    let p = pill(None, clip(&label, LABEL_CHARS), choosable, open);
    if !choosable {
        return p;
    }
    let mut list: Vec<ModelInfo> = models.iter().map(|m| (*m).clone()).collect();
    list.sort_by_key(|m| m.name.to_lowercase());
    let chosen = aim.model.clone();
    // Keep the live selection near the top so long catalogues stay usable.
    if let Some(ref c) = chosen {
        if let Some(i) = list
            .iter()
            .position(|m| m.id == c.model_id && m.provider_id == c.provider_id)
        {
            let selected = list.remove(i);
            list.insert(0, selected);
        }
    }
    let asid = aim.asid();
    let app = app.clone();
    let searchable = list.len() > MODEL_SEARCH_AFTER;
    let height = menu_height(list.len().clamp(1, MAX_MODELS), 0)
        + if searchable { MENU_SEARCH_HEIGHT } else { 0.0 }
        + if list.len() > MAX_MODELS { 18.0 } else { 0.0 };
    host(MenuId::Model, side, p, (300.0, height), move || {
        let query = Cmp::get().menu_query.get();
        let matched: Vec<ModelInfo> = list
            .iter()
            .filter(|m| model_matches(m, &query))
            .cloned()
            .collect();
        let total = matched.len();
        let mut menu = div().flex_col().min_w(260.0);
        if searchable {
            menu = menu.child(model_search_field(&app));
        }
        if total == 0 {
            return menu.child(
                div().class("mq-menu-dim").child(
                    ui_text(app.strings.no_model_matches())
                        .size(12.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        for model in matched.iter().take(MAX_MODELS).cloned() {
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
            let keep_variant = chosen
                .as_ref()
                .filter(|c| c.model_id == model.id && c.provider_id == model.provider_id)
                .and_then(|c| c.variant.clone());
            menu = menu.child(action_row(label, selected, move || {
                let model = ModelRef {
                    provider_id: model.provider_id.clone(),
                    model_id: model.id.clone(),
                    variant: keep_variant.clone(),
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

/// Remember the live choice for this agent+folder, and show it on the pill
/// before the next `SessionUpdated` arrives.
fn remember_live(app: &App, model: Option<&ModelRef>, mode: Option<&str>) {
    let Some(header) = app.bindings.header.get() else {
        return;
    };
    let Some(cwd) = header.directory.clone() else {
        return;
    };
    let agent = header.agent_id.clone();
    let model_s = model.or(header.model.as_ref()).map(super::model_to_string);
    let mode_s = mode.map(str::to_string).or_else(|| header.mode.clone());
    app.send(Command::Edit(crate::config::ConfigEdit::Remember {
        agent,
        cwd,
        model: model_s,
        mode: mode_s,
    }));
}

fn patch_header_model(app: &App, model: ModelRef) {
    if let Some(header) = app.bindings.header.get() {
        let mut info = (*header).clone();
        info.model = Some(model);
        app.bindings.header.signal().set(Some(Arc::new(info)));
    }
}

fn patch_header_mode(app: &App, mode: String) {
    if let Some(header) = app.bindings.header.get() {
        let mut info = (*header).clone();
        info.mode = Some(mode);
        app.bindings.header.signal().set(Some(Arc::new(info)));
    }
}

/// The model (and its effort) the session or the start page will use.
fn choose_model(app: &App, asid: Option<&muqun_gateway::agents::AgentSessionId>, model: ModelRef) {
    match asid {
        Some(asid) => {
            remember_live(app, Some(&model), None);
            patch_header_model(app, model.clone());
            app.send(Command::Compose(ComposeCommand::SwitchModel {
                asid: asid.clone(),
                model,
            }));
        }
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
    let choosable = !modes.is_empty();
    let open = Cmp::get().menu.get() == Some(MenuId::Mode);
    let p = pill(
        Some(glyph_box(Glyph::Document)),
        clip(&label, LABEL_CHARS),
        choosable,
        open,
    );
    if !choosable {
        return p;
    }
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
            menu = menu.child(action_row(
                div().child(ui_text(label).size(13.0).no_wrap().no_cursor()),
                selected,
                move || {
                    match &asid {
                        Some(asid) => {
                            remember_live(&app, None, Some(&id));
                            patch_header_mode(&app, id.clone());
                            app.send(Command::Compose(ComposeCommand::SwitchMode {
                                asid: asid.clone(),
                                mode: id.clone(),
                            }));
                        }
                        None => {
                            let cmp = Cmp::get();
                            let mut choice = (*cmp.start.get()).clone();
                            choice.mode = Some(id.clone());
                            cmp.start.signal().set(Arc::new(choice));
                        }
                    }
                    close_menu();
                },
            ));
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
            menu = menu.child(action_row(
                div().child(mono_text(variant.clone()).size(12.0).no_wrap().no_cursor()),
                selected,
                move || {
                    choose_model(
                        &app,
                        asid.as_ref(),
                        ModelRef {
                            variant: Some(variant.clone()),
                            ..base.clone()
                        },
                    );
                    close_menu();
                },
            ));
        }
        menu
    })
}

/// Compact git control: opens the right column on the Git tab, no label.
fn git_toggle(app: &App, aim: &Aim) -> Div {
    let open = app.bindings.diff_panel.get()
        && app.bindings.right_tab.get() == crate::model::view::RightTab::Git;
    let has_git = aim
        .cwd
        .as_ref()
        .is_some_and(|cwd| Cmp::get().branches.get().contains_key(cwd));
    if !has_git && aim.place == Place::Start {
        return div();
    }
    let app = app.clone();
    let mut p = pill(Some(glyph_box(Glyph::Branch)), String::new(), false, open)
        .cursor_pointer()
        .on_click(move |_| {
            crate::ui::shell::toggle_right(&app, crate::model::view::RightTab::Git);
        });
    if open {
        p = p.class("mq-pill--open");
    }
    p
}

#[allow(dead_code)] // Branch switching moves into the right git column.
fn branch_pill(app: &App, aim: &Aim, side: Side) -> Option<Div> {
    let cwd = aim.cwd.clone()?;
    let info = Cmp::get().branches.get().get(&cwd).cloned()?;
    let branch = info.current.clone()?;
    let open = Cmp::get().menu.get() == Some(MenuId::Branch);
    let p = pill(
        Some(glyph_box(Glyph::Branch)),
        clip(&branch, LABEL_CHARS),
        true,
        open,
    );
    let on_start = aim.place == Place::Start;
    let want_worktree = on_start && Cmp::get().start.get().worktree;
    let locals: Vec<String> = info
        .local
        .iter()
        .filter(|b| *b != &branch)
        .cloned()
        .collect();
    let searchable = locals.len() > MODEL_SEARCH_AFTER;
    let shown = locals.len().min(MAX_BRANCHES);
    let extra = locals.len().saturating_sub(MAX_BRANCHES);
    let rows = 1 + shown.max(1) + usize::from(extra > 0) + 1;
    let seps = 1 + usize::from(shown > 0 || extra > 0);
    let height = menu_height(rows, seps) + if searchable { MENU_SEARCH_HEIGHT } else { 0.0 };
    let app = app.clone();
    Some(host(MenuId::Branch, side, p, (300.0, height), move || {
        let query = Cmp::get().menu_query.get();
        let matched: Vec<String> = locals
            .iter()
            .filter(|name| query.is_empty() || name.to_lowercase().contains(query.as_str()))
            .cloned()
            .collect();
        let shown = matched.len().min(MAX_BRANCHES);
        let extra = matched.len().saturating_sub(MAX_BRANCHES);
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
        let mut menu = div()
            .flex_col()
            .min_w(280.0)
            .child(menu_row(current, true, false, false));
        if searchable {
            menu = menu.child(branch_search_field(&app));
        }
        if shown > 0 || extra > 0 {
            menu = menu.child(separator());
        }
        for name in matched.iter().take(MAX_BRANCHES) {
            let app = app.clone();
            let cwd = cwd.clone();
            let name = name.clone();
            menu = menu.child(action_row(
                div().child(mono_text(name.clone()).size(12.0).no_wrap().no_cursor()),
                false,
                move || {
                    // Live checkout of the project root — same path on Home and
                    // in a session; failures surface through the composer error.
                    app.send(Command::Compose(ComposeCommand::Checkout {
                        cwd: cwd.clone(),
                        branch: name.clone(),
                    }));
                    close_menu();
                },
            ));
        }
        if extra > 0 {
            menu = menu.child(
                div().class("mq-menu-dim").child(
                    ui_text(format!("+{extra}"))
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        menu = menu.child(separator());
        // A new worktree is how to branch off without moving the agent's
        // checkout; offered on the start page, and as "new session" while
        // one is already open.
        let worktree = if on_start {
            action_row(
                div().child(
                    ui_text(app.strings.new_worktree())
                        .size(13.0)
                        .no_wrap()
                        .no_cursor(),
                ),
                want_worktree,
                move || {
                    let cmp = Cmp::get();
                    let mut choice = (*cmp.start.get()).clone();
                    choice.worktree = !choice.worktree;
                    cmp.start.signal().set(Arc::new(choice));
                    close_menu();
                },
            )
        } else {
            let app = app.clone();
            let cwd = cwd.clone();
            action_row(
                div().child(
                    ui_text(app.strings.new_session_worktree())
                        .size(13.0)
                        .no_wrap()
                        .no_cursor(),
                ),
                false,
                move || {
                    app.open_draft_in(std::path::Path::new(&cwd));
                    let cmp = Cmp::get();
                    let mut choice = (*cmp.start.get()).clone();
                    choice.worktree = true;
                    cmp.start.signal().set(Arc::new(choice));
                    close_menu();
                },
            )
        };
        menu.child(worktree)
    }))
}

fn branch_search_field(app: &App) -> Div {
    let filter = Cmp::get().model_filter.clone();
    let placeholder = app.strings.branch_search().to_string();
    if let Ok(mut data) = filter.lock() {
        data.placeholder = placeholder;
    }
    div()
        .class("mq-menu-search")
        .w_full()
        .h(MENU_SEARCH_HEIGHT)
        .flex_row()
        .items_center()
        .child(
            div().flex_1().min_w(0.0).child(
                input(&filter)
                    .size(InputSize::Small)
                    .w_full()
                    .border_width(0.0)
                    .border_colors(
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                    )
                    .bg_colors(
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                        blinc_core::Color::TRANSPARENT,
                    )
                    .on_change(move |text: &str| {
                        let next = text.trim().to_lowercase();
                        let cmp = Cmp::get();
                        if cmp.menu_query.get() != next {
                            cmp.menu_query.signal().set(next);
                            cmp.menu_cursor.signal().set(0);
                        }
                    }),
            ),
        )
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

    #[test]
    fn model_search_matches_name_id_and_provider() {
        let model = ModelInfo {
            id: "gpt-5".into(),
            provider_id: "openai".into(),
            name: "GPT-5".into(),
            family: None,
            limit: None,
            variants: None,
            cost: None,
            enabled: true,
            status: None,
        };
        assert!(model_matches(&model, ""));
        assert!(model_matches(&model, "gpt"));
        assert!(model_matches(&model, "OPENAI"));
        assert!(model_matches(&model, "5"));
        assert!(!model_matches(&model, "claude"));
    }
}
