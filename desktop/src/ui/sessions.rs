//! The sidebar (architecture §6.0‴ A, §6.1b′): a search field, the PROJECT →
//! SESSION tree, and the Inbox and Settings at the bottom. Below 960 px of
//! window it folds to a 56 px rail of icons ([`rail`]).
//!
//! The tree is a windowed list: [`projects::flatten`] turns the projects
//! into fixed-height rows, and only the rows within a viewport of the
//! visible ones are built; two spacers stand in for the rest
//! (`timeline/window.rs`'s [`Model`], with every height known up front, so
//! there is no measuring and no anchoring to do). The list is rebuilt only
//! when its layout changes -- sessions listed or reordered, folders resolved,
//! a project opened, a search typed, the window scrolled past its margin.
//! What a session row shows lives in that row's keyed state
//! (`view::row_state`), so a status change, a new title or the selection
//! moving rebuilds one row, not the list.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use blinc_cn::prelude::{dropdown_menu_custom, icons, to_svg, ColorToken, ThemeState};
use blinc_core::State;
use blinc_layout::prelude::*;
use blinc_layout::widgets::text_input::{text_input_data_with_placeholder, SharedTextInputData};
use muqun_gateway::agents::{AgentSessionInfo, AgentSessionStatus};
use muqun_gateway::untitled_placeholder;

use super::agents;
use super::text::ui_text;
use super::timeline::window::Model;
use super::App;
use crate::config::ConfigEdit;
use crate::model::projects::{self, Project, ProjectPrefs, TreeOptions, TreeRow};
use crate::model::view::{local_day, row_state, SessionRow};
use crate::platform::keys;

/// Heights of the tree's rows (architecture §6.0‴ A).
pub const PROJECT_ROW: f32 = 32.0;
pub const SESSION_ROW: f32 = 30.0;
const GAP_ROW: f32 = 8.0;
/// The search field, and the block it sits in.
const SEARCH_FIELD: f32 = 36.0;
const SEARCH_BLOCK: f32 = SEARCH_FIELD + 12.0 + 8.0;
/// One bottom entry (Inbox, Settings), and the block of both.
const ENTRY_ROW: f32 = 36.0;
const BOTTOM_BLOCK: f32 = 2.0 * ENTRY_ROW + 1.0 + 16.0;
/// The folded sidebar.
pub const RAIL_WIDTH: f32 = 56.0;
const RAIL_ITEM: f32 = 40.0;
/// Projects open by default, and sessions a project shows before "More".
const DEFAULT_OPEN: usize = 3;
const PER_PROJECT: usize = 5;
/// Viewport height before the first layout.
const DEFAULT_VIEWPORT: f32 = 800.0;

/// The sidebar's own state: UI-owned, created once (main thread, Blinc up).
struct Side {
    /// Lowercased search text.
    query: State<String>,
    search: SharedTextInputData,
    /// Projects opened or closed by hand, by id.
    open: State<Arc<HashMap<String, bool>>>,
    /// Projects opened past "More", by id.
    full: State<Arc<HashSet<String>>>,
    /// Pins, hidden projects and names, as the owner set them; written to
    /// the config as they change.
    prefs: State<Arc<BTreeMap<String, ProjectPrefs>>>,
    /// Bumped when the scroll leaves the built window.
    epoch: State<u64>,
    physics: SharedScrollPhysics,
    built: Mutex<Built>,
}

/// The tree as last laid out, for scrolling and for the rows' handlers.
#[derive(Default)]
struct Built {
    model: Model,
    window: Range<usize>,
}

fn side(app: &App) -> &'static Side {
    static SIDE: OnceLock<Side> = OnceLock::new();
    SIDE.get_or_init(|| {
        let ctx = blinc_core::BlincContextState::get();
        let prefs = app.config.get().ui.projects;
        let placeholder = app.strings.search().to_string();
        // QA: start with this search typed (`MUQUN_QA_SIDEBAR_QUERY`).
        let typed = std::env::var("MUQUN_QA_SIDEBAR_QUERY").unwrap_or_default();
        let query = typed.trim().to_lowercase();
        Side {
            query: ctx.use_state_keyed("mq.side.query", move || query.clone()),
            search: ctx
                .use_state_keyed("mq.side.search", move || {
                    let data = text_input_data_with_placeholder(placeholder.clone());
                    if let Ok(mut d) = data.lock() {
                        d.cursor = typed.chars().count();
                        d.value = typed.clone();
                    }
                    data
                })
                .get(),
            open: ctx.use_state_keyed("mq.side.open", || Arc::new(HashMap::new())),
            full: ctx.use_state_keyed("mq.side.full", || Arc::new(HashSet::new())),
            prefs: ctx.use_state_keyed("mq.side.prefs", move || Arc::new(prefs.clone())),
            epoch: ctx.use_state_keyed("mq.side.epoch", || 0u64),
            physics: Arc::new(Mutex::new(ScrollPhysics::new(ScrollConfig::no_bounce()))),
            built: Mutex::new(Built::default()),
        }
    })
}

/// Put the caret in the sidebar's search field (Ctrl/⌘K, the top bar's
/// search icon). Nothing while the sidebar is folded to its rail.
pub fn focus_search() {
    if let Some(side) = SIDE_READY.get() {
        blinc_layout::widgets::text_input::focus_text_input(side);
    }
}

/// The search field's state once the sidebar has been built.
static SIDE_READY: OnceLock<SharedTextInputData> = OnceLock::new();

fn tinted(path: &str, size: f32, token: ColorToken) -> Svg {
    svg(to_svg(path, size))
        .size(size, size)
        .tint(ThemeState::get().color(token))
        .flex_shrink_0()
}

/// `row`, still empty, given the raised surface while it is the selected
/// session or the open page: a first child filling it, so everything added
/// after paints over it.
///
/// A child, not the row's own background or a class: a keyed row that
/// renders again is patched in place, and Blinc 0.5.1 applies neither a
/// changed background nor a changed class of a patched element (a child
/// that comes or goes is a structural change, and is built fresh).
fn raised(row: Div, on: bool) -> Div {
    if !on {
        return row;
    }
    row.child(
        div()
            .class("mq-raised")
            .absolute()
            .left(0.0)
            .top(0.0)
            .right(0.0)
            .bottom(0.0)
            .bg(ThemeState::get().color(ColorToken::SurfaceOverlay)),
    )
}

/// A lucide icon at `size` in the secondary text colour.
pub fn icon(path: &str, size: f32) -> Svg {
    tinted(path, size, ColorToken::TextSecondary)
}

pub fn sidebar(app: &App, height: f32) -> Div {
    qa(app);
    qa_states(app);
    let tree_height = (height - SEARCH_BLOCK - BOTTOM_BLOCK).max(0.0);
    div()
        .class("mq-sidebar")
        .w_full()
        .h(height)
        .flex_col()
        .child(search_field(app))
        .child(
            div()
                .class("mq-tree")
                .w_full()
                .h(tree_height)
                .flex_shrink_0()
                .child(tree(app, tree_height)),
        )
        .child(
            div()
                .class("mq-side-bottom")
                .w_full()
                .flex_col()
                .flex_shrink_0()
                .child(inbox_entry(app, false))
                .child(settings_entry(app, false)),
        )
}

/// QA (`MUQUN_QA_SIDEBAR=<secs>`): that long after start, open every
/// project all the way, then scroll the tree a viewport a second to the
/// bottom, logging the built window at each stop (`qa: sidebar`), so the
/// windowing can be watched headed without synthetic input.
fn qa(app: &App) {
    static ONCE: OnceLock<()> = OnceLock::new();
    let Some(after) = std::env::var("MUQUN_QA_SIDEBAR")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    if ONCE.set(()).is_err() {
        return;
    }
    let Some(scheduler) = blinc_animation::try_get_scheduler() else {
        return;
    };
    let app = app.clone();
    let started = std::time::Instant::now();
    let mut opened = false;
    let mut next = std::time::Duration::from_secs(after);
    let id: Arc<Mutex<Option<blinc_animation::TickCallbackId>>> = Arc::default();
    let own = id.clone();
    *id.lock().unwrap() = scheduler.register_tick_callback(move |_| {
        if started.elapsed() < next {
            return;
        }
        next = started.elapsed() + std::time::Duration::from_secs(1);
        let s = side(&app);
        if !opened {
            opened = true;
            let projects = current_projects(&app);
            let ids: HashSet<String> = projects.iter().map(|p| p.id.clone()).collect();
            let open: HashMap<String, bool> = ids.iter().map(|i| (i.clone(), true)).collect();
            let sessions: usize = projects.iter().map(|p| p.sessions.len()).sum();
            tracing::info!(
                projects = projects.len(),
                sessions,
                "qa: sidebar opened all"
            );
            blinc_core::reactive::batch_stateful_deps(|| {
                s.open.signal().set(Arc::new(open));
                s.full.signal().set(Arc::new(ids));
            });
            return;
        }
        let (top, vh, max) = {
            let p = s.physics.lock().unwrap();
            (
                -p.offset_y,
                p.viewport_height,
                (p.content_height - p.viewport_height).max(0.0),
            )
        };
        let to = (top + vh).min(max);
        s.physics.lock().unwrap().offset_y = -to;
        let started = std::time::Instant::now();
        scrolled(&app);
        let built = s.built.lock().unwrap();
        tracing::info!(
            top = to as u32,
            rows = built.model.heights.len(),
            window = ?built.window,
            us = started.elapsed().as_micros() as u64,
            "qa: sidebar scrolled"
        );
        if to >= max {
            tracing::info!("qa: sidebar at the bottom");
            if let (Some(id), Some(scheduler)) = (
                own.lock().unwrap().take(),
                blinc_animation::try_get_scheduler(),
            ) {
                scheduler.remove_tick_callback(id);
            }
        }
    });
}

/// QA (`MUQUN_QA_SIDEBAR_STATES=<secs>`): that long after start, show the
/// first five sessions of the top project as running, waiting, failed, idle
/// and stopped, by injecting the events the core would send (nothing reaches
/// an agent), for a close-up of every badge.
fn qa_states(app: &App) {
    use muqun_gateway::agents::{AgentDomainEvent, AgentSessionId, PermissionRequest};
    static ONCE: OnceLock<()> = OnceLock::new();
    let Some(after) = std::env::var("MUQUN_QA_SIDEBAR_STATES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    else {
        return;
    };
    if ONCE.set(()).is_err() {
        return;
    }
    let Some(scheduler) = blinc_animation::try_get_scheduler() else {
        return;
    };
    let app = app.clone();
    let started = std::time::Instant::now();
    let id: Arc<Mutex<Option<blinc_animation::TickCallbackId>>> = Arc::default();
    let own = id.clone();
    *id.lock().unwrap() = scheduler.register_tick_callback(move |_| {
        if started.elapsed() < std::time::Duration::from_secs(after) {
            return;
        }
        if let (Some(id), Some(scheduler)) = (
            own.lock().unwrap().take(),
            blinc_animation::try_get_scheduler(),
        ) {
            scheduler.remove_tick_callback(id);
        }
        let Some(project) = current_projects(&app).into_iter().find(|p| !p.hidden) else {
            return;
        };
        let seq = 1u64 << 40;
        for (i, asid) in project.sessions.iter().take(5).enumerate() {
            let id = AgentSessionId(asid.clone());
            let status = match i {
                0 => Some(AgentSessionStatus::Busy),
                2 => Some(AgentSessionStatus::Failed),
                3 => Some(AgentSessionStatus::Idle),
                4 => Some(AgentSessionStatus::Interrupted),
                _ => None,
            };
            let event = match status {
                Some(status) => AgentDomainEvent::StatusChanged {
                    asid: id,
                    status,
                    error: None,
                    seq,
                },
                None => {
                    let request: PermissionRequest = serde_json::from_value(serde_json::json!({
                        "id": "qa-badge", "asid": asid, "action": "bash",
                        "resources": ["ls"], "prompt": "QA: run ls?", "options": []
                    }))
                    .expect("a permission request");
                    AgentDomainEvent::PermissionPending {
                        asid: id,
                        request,
                        seq,
                    }
                }
            };
            app.drain.inject(crate::model::inbox::Update::event(event));
        }
        tracing::info!(project = %project.id, "qa: sidebar states injected");
    });
}

/// The folded sidebar: the most recent sessions as agent marks, then the
/// Inbox and Settings.
pub fn rail(app: &App, height: f32) -> Div {
    let listing = app.bindings.listing.clone();
    let fits = ((height - BOTTOM_BLOCK - 16.0) / RAIL_ITEM)
        .floor()
        .max(0.0) as usize;
    let rail_app = app.clone();
    let recent = stateful_with_key::<NoState>("mq-rail-list")
        .deps([listing.signal_id()])
        .on_state(move |_| {
            let mut list = div()
                .class("mq-rail-list")
                .w_full()
                .flex_col()
                .items_center();
            for session in listing.get().sessions.iter().take(fits) {
                list = list.child(rail_item(&rail_app, &session.asid));
            }
            list
        });
    div()
        .class("mq-sidebar")
        .class("mq-rail")
        .w(RAIL_WIDTH)
        .h(height)
        .flex_col()
        .child(
            div()
                .w_full()
                .flex_1()
                .min_h(0.0)
                .overflow_clip()
                .child(recent),
        )
        .child(
            div()
                .class("mq-side-bottom")
                .w_full()
                .flex_col()
                .flex_shrink_0()
                .child(inbox_entry(app, true))
                .child(settings_entry(app, true)),
        )
}

fn rail_item(app: &App, asid: &str) -> impl ElementBuilder {
    let state = row_state(asid);
    let selected = app.bindings.selected.clone();
    let asid_key = asid.to_string();
    let app = app.clone();
    // `selected` is a dep of its own: baking `active` into the row state alone
    // missed the highlight when the row was built before `drain.select` wrote it.
    stateful_with_key::<NoState>(format!("mq-rrow.{asid}"))
        .deps([state.signal_id(), selected.signal_id()])
        .on_state(move |_| {
            let Some(row) = state.get() else {
                return div();
            };
            let active = selected.get().as_deref() == Some(asid_key.as_str());
            let asid = muqun_gateway::agents::AgentSessionId(row.asid.clone());
            let click = app.clone();
            raised(div(), active)
                .class("mq-rail-item")
                .relative()
                .w(RAIL_ITEM)
                .h(RAIL_ITEM)
                .flex_row()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(badged_mark(&row.agent_id, 20.0, row.status, row.waiting))
                .on_click(move |_| click.open_session(&asid))
        })
}

fn search_field(app: &App) -> Div {
    let s = side(app);
    let _ = SIDE_READY.set(s.search.clone());
    let query = s.query.clone();
    div()
        .class("mq-side-search-wrap")
        .w_full()
        .flex_shrink_0()
        .child(
            div()
                .class("mq-side-search")
                .w_full()
                .h(SEARCH_FIELD)
                .flex_row()
                .items_center()
                .child(icon(icons::SEARCH, 14.0))
                .child(
                    div().flex_1().min_w(0.0).child(
                        blinc_cn::prelude::input(&s.search)
                            .size(blinc_cn::prelude::InputSize::Small)
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
                                let text = text.trim().to_lowercase();
                                if query.get() != text {
                                    query.signal().set(text);
                                }
                            }),
                    ),
                )
                .child(
                    div().class("mq-kbd").child(
                        ui_text(keys::label(keys::SEARCH))
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                ),
        )
}

/// The tree's scroll and its windowed list.
fn tree(app: &App, height: f32) -> impl ElementBuilder {
    let s = side(app);
    let b = &app.bindings;
    let deps = [
        b.listing.signal_id(),
        b.dirs.signal_id(),
        b.known.signal_id(),
        s.query.signal_id(),
        s.open.signal_id(),
        s.full.signal_id(),
        s.prefs.signal_id(),
        s.epoch.signal_id(),
    ];
    let list_app = app.clone();
    let list = stateful_with_key::<NoState>("mq-tree-list")
        .deps(deps)
        .on_state(move |_| build_tree(&list_app));
    let scroll_app = app.clone();
    Scroll::with_physics(s.physics.clone())
        .w_full()
        .h(height)
        .on_scroll(move |_| scrolled(&scroll_app))
        .child(list)
}

/// (scroll top, viewport height).
fn viewport(physics: &SharedScrollPhysics) -> (f32, f32) {
    let p = physics.lock().unwrap();
    let vh = if p.viewport_height > 0.0 {
        p.viewport_height
    } else {
        DEFAULT_VIEWPORT
    };
    (-p.offset_y, vh)
}

/// Rows built beyond the viewport on each side, and how close the viewport
/// may come to the built window's edge before it is rebuilt.
fn buffer(vh: f32) -> (f32, f32) {
    (vh, vh * 0.4)
}

fn scrolled(app: &App) {
    let s = side(app);
    let (top, vh) = viewport(&s.physics);
    let stale = {
        let built = s.built.lock().unwrap();
        !built.model.covers(&built.window, top, vh, buffer(vh).1)
    };
    if stale {
        s.epoch.signal().set(s.epoch.get() + 1);
    }
}

fn row_height(row: &TreeRow) -> f32 {
    match row {
        TreeRow::Gap => GAP_ROW,
        TreeRow::Project { .. } => PROJECT_ROW,
        TreeRow::Session { .. } | TreeRow::More { .. } => SESSION_ROW,
    }
}

/// The projects as the sidebar shows them now.
pub fn current_projects(app: &App) -> Vec<Project> {
    let b = &app.bindings;
    let listing = b.listing.get();
    let dirs = b.dirs.get();
    let known = b.known.get();
    let prefs = side(app).prefs.get();
    let recent: Vec<std::path::PathBuf> = app.config.get().general.last_cwd.into_values().collect();
    let home = dirs::home_dir();
    projects::derive(&projects::Sources {
        sessions: &listing.sessions,
        dirs: &dirs,
        known: &known,
        recent: &recent,
        prefs: &prefs,
        home: home.as_deref(),
    })
}

fn build_tree(app: &App) -> Div {
    let started = std::time::Instant::now();
    let s = side(app);
    let projects = current_projects(app);
    let query = s.query.get();
    // Titles only matter to a search; read, not depended on (a title
    // changing under a search does not re-run it until the next keystroke).
    let titles: HashMap<String, String> = if query.is_empty() {
        HashMap::new()
    } else {
        app.bindings
            .sessions
            .get()
            .iter()
            .map(|i| (i.asid.0.clone(), title_of(app, i)))
            .collect()
    };
    let open = s.open.get();
    let full = s.full.get();
    let rows = projects::flatten(
        &projects,
        &TreeOptions {
            query: &query,
            open: &open,
            full: &full,
            default_open: DEFAULT_OPEN,
            per_project: PER_PROJECT,
        },
        |asid| titles.get(asid).cloned(),
    );
    let mut model = Model {
        heights: rows.iter().map(row_height).collect(),
        offsets: Vec::new(),
    };
    model.resum();
    let (top, vh) = viewport(&s.physics);
    let window = model.window(top, vh, buffer(vh).0);
    let (above, below) = model.spacers(&window);
    let by_id: HashMap<&str, &Project> = projects.iter().map(|p| (p.id.as_str(), p)).collect();
    let mut list = div().class("mq-tree-list").w_full().flex_col();
    if rows.is_empty() {
        let empty = if query.is_empty() {
            app.strings.no_sessions()
        } else {
            app.strings.no_match()
        };
        list = list.child(div().class("mq-empty").child(ui_text(empty).size(13.0)));
    }
    list = list.child(div().w_full().h(above).flex_shrink_0());
    for row in &rows[window.clone()] {
        list = list.child(match row {
            TreeRow::Gap => div().w_full().h(GAP_ROW).flex_shrink_0(),
            TreeRow::Project { id, open } => match by_id.get(id.as_str()) {
                Some(project) => project_row(app, project, *open),
                None => div().h(PROJECT_ROW),
            },
            TreeRow::Session { asid, .. } => session_row(app, asid),
            TreeRow::More { project, hidden } => more_row(app, project, *hidden),
        });
    }
    list = list.child(div().w_full().h(below).flex_shrink_0());
    tracing::debug!(
        target: "muqun::sidebar",
        projects = projects.len(),
        rows = rows.len(),
        built = window.len(),
        us = started.elapsed().as_micros() as u64,
        "tree built"
    );
    *s.built.lock().unwrap() = Built { model, window };
    list
}

/// Make sure `asid`'s row is in the tree: open its project, and past
/// "More" when it is not among the first sessions. Writes only what has to
/// change, so opening a session already in view rebuilds nothing.
pub fn reveal(app: &App, asid: &str) {
    let s = side(app);
    if !s.query.get().is_empty() {
        return;
    }
    let projects = current_projects(app);
    let Some((place, project)) = projects
        .iter()
        .filter(|p| !p.hidden)
        .enumerate()
        .find(|(_, p)| p.sessions.iter().any(|a| a == asid))
    else {
        return;
    };
    let mut open = (*s.open.get()).clone();
    let is_open = open
        .get(&project.id)
        .copied()
        .unwrap_or(place < DEFAULT_OPEN);
    let position = project.sessions.iter().position(|a| a == asid).unwrap_or(0);
    let mut full = (*s.full.get()).clone();
    let open_it = !is_open;
    let widen = position >= PER_PROJECT && !full.contains(&project.id);
    open.insert(project.id.clone(), true);
    if widen {
        full.insert(project.id.clone());
    }
    // Where the row will be, and the scroll that shows it.
    let rows = projects::flatten(
        &projects,
        &TreeOptions {
            query: "",
            open: &open,
            full: &full,
            default_open: DEFAULT_OPEN,
            per_project: PER_PROJECT,
        },
        |_| None,
    );
    let mut model = Model {
        heights: rows.iter().map(row_height).collect(),
        offsets: Vec::new(),
    };
    model.resum();
    let at = rows
        .iter()
        .position(|r| matches!(r, TreeRow::Session { asid: a, .. } if a == asid));
    let mut scrolled_to = false;
    if let Some(at) = at {
        let (top, vh) = viewport(&s.physics);
        let y = model.top(at);
        if y < top || y + SESSION_ROW > top + vh {
            let max = (model.total() - vh).max(0.0);
            let mut p = s.physics.lock().unwrap();
            p.offset_y = -(y - vh / 3.0).clamp(0.0, max);
            p.velocity_y = 0.0;
            scrolled_to = true;
        }
    }
    blinc_core::reactive::batch_stateful_deps(|| {
        if open_it {
            s.open.signal().set(Arc::new(open));
        }
        if widen {
            s.full.signal().set(Arc::new(full));
        }
        if scrolled_to && !open_it && !widen {
            s.epoch.signal().set(s.epoch.get() + 1);
        }
    });
}

/// Open or close a project by hand.
fn toggle(app: &App, id: &str, open: bool) {
    let state = &side(app).open;
    let mut map = (*state.get()).clone();
    map.insert(id.to_string(), !open);
    state.signal().set(Arc::new(map));
}

/// Change what the owner chose for a project, and remember it.
fn set_prefs(app: &App, root: &str, change: impl FnOnce(&mut ProjectPrefs)) {
    let state = &side(app).prefs;
    let mut map = (*state.get()).clone();
    let mut prefs = map.get(root).cloned().unwrap_or_default();
    change(&mut prefs);
    if prefs.is_default() {
        map.remove(root);
    } else {
        map.insert(root.to_string(), prefs.clone());
    }
    state.signal().set(Arc::new(map));
    app.send(crate::model::commands::Command::Edit(ConfigEdit::Project {
        root: root.to_string(),
        prefs,
    }));
}

fn project_row(app: &App, project: &Project, open: bool) -> Div {
    let strings = app.strings;
    let mut name = div()
        .class("mq-prow-name")
        .flex_row()
        .items_center()
        .flex_1()
        .min_w(0.0)
        .overflow_clip();
    if project.is_no_folder() {
        name = name.child(
            ui_text(strings.no_folder())
                .size(13.0)
                .medium()
                .no_wrap()
                .no_cursor(),
        );
    } else if project.is_other() {
        name = name.child(
            ui_text(strings.other_count(project.sessions.len()))
                .size(13.0)
                .medium()
                .no_wrap()
                .no_cursor(),
        );
    } else {
        let (parent, base) = &project.display;
        if !parent.is_empty() {
            name = name.child(
                div()
                    .class("mq-prow-parent")
                    .child(ui_text(parent.clone()).size(13.0).no_wrap().no_cursor()),
            );
        }
        name = name.child(
            ui_text(base.clone())
                .size(13.0)
                .semibold()
                .no_wrap()
                .no_cursor(),
        );
        if project.missing {
            name = name.child(
                div().class("mq-prow-missing").child(
                    ui_text(strings.folder_missing())
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
    }
    let toggle_app = app.clone();
    let id = project.id.clone();
    let main = div()
        .class("mq-prow-main")
        .absolute()
        .left(0.0)
        .top(0.0)
        .right(0.0)
        .bottom(0.0)
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(super::icons::chevron(open))
        .child(name)
        .on_click(move |_| toggle(&toggle_app, &id, open));
    let mut row = div()
        .class("mq-prow")
        .class_when("mq-prow--missing", project.missing)
        .relative()
        .w_full()
        .h(PROJECT_ROW)
        .flex_shrink_0()
        .child(main);
    if !project.is_no_folder() && !project.is_other() {
        // A sibling of the clickable row, not a child: a click on an action
        // must not also open or close the project (clicks bubble).
        row = row.child(
            div()
                .class("mq-prow-actions")
                .absolute()
                .right(4.0)
                .top(0.0)
                .bottom(0.0)
                .flex_row()
                .items_center()
                .child(new_here_button(app, project))
                .child(project_menu(app, project)),
        );
    }
    row
}

fn new_here_button(app: &App, project: &Project) -> Div {
    let app = app.clone();
    let root = project.root.clone();
    div()
        .class("mq-icon-button")
        .class("mq-icon-button--small")
        .w(22.0)
        .h(22.0)
        .flex_row()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(icon(icons::PLUS, 14.0))
        .on_click(move |_| {
            app.open_draft_in(&root);
        })
}

fn project_menu(app: &App, project: &Project) -> impl ElementBuilder {
    let strings = app.strings;
    let editor = super::shell::editor_state(app).get();
    let (open_app, pin_app, hide_app) = (app.clone(), app.clone(), app.clone());
    let root = project.root.clone();
    let (pin_id, hide_id) = (project.id.clone(), project.id.clone());
    let pinned = project.pinned;
    let hidden = project.hidden;
    dropdown_menu_custom(|_open| {
        div()
            .class("mq-icon-button")
            .class("mq-icon-button--small")
            .w(22.0)
            .h(22.0)
            .flex_row()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .child(icon(icons::ELLIPSIS, 14.0))
    })
    .min_width(180.0)
    .item(strings.open_in(editor), move || {
        super::shell::open_in(&open_app, editor, &root)
    })
    .item(
        if pinned {
            strings.unpin()
        } else {
            strings.pin()
        },
        move || set_prefs(&pin_app, &pin_id, |p| p.pinned = !pinned),
    )
    .item(
        if hidden {
            strings.unhide()
        } else {
            strings.hide()
        },
        move || set_prefs(&hide_app, &hide_id, |p| p.hidden = !hidden),
    )
}

fn more_row(app: &App, project: &str, hidden: usize) -> Div {
    let app = app.clone();
    let id = project.to_string();
    let label = app.strings.more_count(hidden);
    div()
        .class("mq-srow")
        .class("mq-srow--more")
        .w_full()
        .h(SESSION_ROW)
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(div().w(6.0).h(6.0).flex_shrink_0())
        .child(ui_text(label).size(13.0).no_wrap().no_cursor())
        .on_click(move |_| {
            let state = &side(&app).full;
            let mut full = (*state.get()).clone();
            full.insert(id.clone());
            state.signal().set(Arc::new(full));
        })
}

fn inbox_entry(app: &App, rail: bool) -> impl ElementBuilder {
    let b = app.bindings.clone();
    let app = app.clone();
    stateful_with_key::<NoState>(if rail {
        "mq-inbox-entry-rail"
    } else {
        "mq-inbox-entry"
    })
    .deps([
        b.attention.signal_id(),
        b.waiting_forms.signal_id(),
        b.inbox_open.signal_id(),
    ])
    .on_state(move |_| {
        let count = b.attention.get().len() + b.waiting_forms.get().len();
        let click = app.clone();
        let mut row = entry(rail, b.inbox_open.get())
            .child(icon(icons::INBOX, 16.0))
            .on_click(move |_| {
                click.bindings.draft.signal().set(None);
                click.bindings.settings_open.signal().set(false);
                click.bindings.inbox_open.signal().set(true);
            });
        if !rail {
            row = row.child(
                div().class("mq-nav-label").flex_1().child(
                    ui_text(app.strings.inbox())
                        .size(13.0)
                        .no_wrap()
                        .no_cursor(),
                ),
            );
        }
        if count > 0 {
            let badge = div()
                .class("mq-badge-count")
                .h(18.0)
                .min_w(18.0)
                .flex_row()
                .items_center()
                .justify_center()
                .child(
                    ui_text(count.to_string())
                        .size(11.0)
                        .semibold()
                        .no_wrap()
                        .no_cursor(),
                );
            row = if rail {
                row.child(div().absolute().right(4.0).top(2.0).child(badge))
            } else {
                row.child(badge)
            };
        }
        row
    })
}

fn settings_entry(app: &App, rail: bool) -> impl ElementBuilder {
    let open = app.bindings.settings_open.clone();
    let app = app.clone();
    stateful_with_key::<NoState>(if rail {
        "mq-settings-entry-rail"
    } else {
        "mq-settings-entry"
    })
    .deps([open.signal_id()])
    .on_state(move |_| {
        let click = app.clone();
        let mut row = entry(rail, open.get())
            .child(super::icons::settings())
            .on_click(move |_| {
                if click.bindings.settings_open.get() {
                    click.close_settings();
                } else {
                    click.open_settings();
                }
            });
        if !rail {
            row = row
                .child(
                    div().class("mq-nav-label").flex_1().child(
                        ui_text(app.strings.settings())
                            .size(13.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                )
                .child(
                    div().class("mq-kbd").child(
                        ui_text(keys::label(keys::SETTINGS))
                            .size(11.0)
                            .no_wrap()
                            .no_cursor(),
                    ),
                );
        }
        row
    })
}

/// A bottom entry: 36 px, raised while its page is open.
fn entry(rail: bool, open: bool) -> Div {
    let row = raised(div(), open)
        .class("mq-side-entry")
        .relative()
        .h(ENTRY_ROW)
        .flex_row()
        .items_center()
        .cursor_pointer();
    if rail {
        row.class("mq-side-entry--rail")
            .w(RAIL_ITEM)
            .justify_center()
    } else {
        row.w_full()
    }
}

/// Today / Yesterday / This week / Earlier, by local calendar day (Home's
/// summary line uses it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayGroup {
    Today,
    Yesterday,
    ThisWeek,
    Earlier,
}

impl DayGroup {
    /// The group of an update at `updated_ms`, seen at `now_ms`. (Its only
    /// caller was the old Home dashboard; the sidebar's grouping rewrite owns it.)
    #[allow(dead_code)]
    pub fn of(updated_ms: u64, now_ms: u64, utc_offset_s: i64) -> Self {
        Self::of_day(
            local_day(updated_ms, utc_offset_s),
            local_day(now_ms, utc_offset_s),
        )
    }

    /// The group of local day `day` (see [`local_day`]) seen on day `today`.
    pub fn of_day(day: i64, today: i64) -> Self {
        match today - day {
            i64::MIN..=0 => Self::Today,
            1 => Self::Yesterday,
            2..=6 => Self::ThisWeek,
            _ => Self::Earlier,
        }
    }
}

pub fn title_of(app: &App, session: &AgentSessionInfo) -> String {
    display_title(app, &session.asid.0, &session.title)
}

/// A session's title as shown: "Untitled" for none or a placeholder.
pub fn display_title(app: &App, asid: &str, title: &str) -> String {
    if title.trim().is_empty() || untitled_placeholder(asid, title) {
        app.strings.untitled().to_string()
    } else {
        title.to_string()
    }
}

/// What a session's badge says: running, waiting for you, failed -- or
/// nothing, for an idle (or stopped, or unknown) session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Running,
    Waiting,
    Failed,
}

impl Badge {
    pub fn of(status: AgentSessionStatus, waiting: bool) -> Option<Self> {
        if waiting {
            return Some(Self::Waiting);
        }
        match status {
            AgentSessionStatus::Busy | AgentSessionStatus::Retry => Some(Self::Running),
            AgentSessionStatus::Failed => Some(Self::Failed),
            AgentSessionStatus::Interrupted
            | AgentSessionStatus::Idle
            | AgentSessionStatus::Unknown => None,
        }
    }

    fn token(self) -> ColorToken {
        match self {
            // Success, not primary: a theme's primary may be red (Cover
            // Courier light), which would read as failed.
            Self::Running => ColorToken::Success,
            Self::Waiting => ColorToken::Warning,
            Self::Failed => ColorToken::Error,
        }
    }
}

/// The 6 px status dot inside a 1.5 px ring of the sidebar's own colour, so
/// it reads on any mark. Coloured inline from the theme, like [`raised`]: a
/// row whose status changes is patched in place.
pub fn badge(badge: Badge) -> Div {
    let theme = ThemeState::get();
    // Two discs, the ring under the dot: a border on a rounded box is not
    // drawn reliably here.
    div()
        .class("mq-sbadge")
        .bg(theme.color(ColorToken::Surface))
        .w(BADGE)
        .h(BADGE)
        .rounded(BADGE / 2.0)
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .child(
            div()
                .bg(theme.color(badge.token()))
                .w(BADGE_DOT)
                .h(BADGE_DOT)
                .rounded(BADGE_DOT / 2.0),
        )
}

/// The dot, and the dot with its 1.5 px ring on each side.
const BADGE_DOT: f32 = 6.0;
const BADGE: f32 = 9.0;

/// An agent's brand mark at `size` with the session's badge over its
/// bottom-right corner (none while idle).
pub fn badged_mark(agent: &str, size: f32, status: AgentSessionStatus, waiting: bool) -> Div {
    let mut mark = div()
        .class("mq-badged")
        .relative()
        .w(size)
        .h(size)
        .flex_shrink_0()
        .child(agents::mark(agent, size));
    if let Some(b) = Badge::of(status, waiting) {
        mark = mark.child(
            div()
                .absolute()
                .right(-BADGE / 3.0)
                .bottom(-BADGE / 3.0)
                .child(badge(b)),
        );
    }
    mark
}

/// One session row, rebuilt when its own state or the selection changes.
fn session_row(app: &App, asid: &str) -> Div {
    let state = row_state(asid);
    let selected = app.bindings.selected.clone();
    let asid_key = asid.to_string();
    let app = app.clone();
    div().w_full().h(SESSION_ROW).flex_shrink_0().child(
        stateful_with_key::<NoState>(format!("mq-srow.{asid}"))
            .deps([state.signal_id(), selected.signal_id()])
            .on_state(move |_| match state.get() {
                Some(row) => {
                    let active = selected.get().as_deref() == Some(asid_key.as_str());
                    render_row(&app, &row, active)
                }
                None => div(),
            }),
    )
}

fn render_row(app: &App, session: &SessionRow, active: bool) -> Div {
    let asid = muqun_gateway::agents::AgentSessionId(session.asid.clone());
    let click = app.clone();
    raised(div(), active)
        .class("mq-srow")
        .relative()
        .w_full()
        .h(SESSION_ROW)
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(badged_mark(
            &session.agent_id,
            16.0,
            session.status,
            session.waiting,
        ))
        .child(
            div().flex_1().min_w(0.0).ml(2.0).overflow_clip().child(
                ui_text(display_title(app, &session.asid, &session.title))
                    .size(13.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        )
        // The relative time, only while the row is hovered
        // (`.mq-srow:hover` in styles.css), over the title's end.
        .child(
            div()
                .class("mq-srow-time")
                .absolute()
                .right(0.0)
                .top(0.0)
                .bottom(0.0)
                .flex_row()
                .items_center()
                .child(
                    ui_text(session.since.clone())
                        .size(11.0)
                        .no_wrap()
                        .no_cursor(),
                ),
        )
        .on_click(move |_| click.open_session(&asid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::projects::{DirInfo, ProjectKind};
    use crate::model::view::ListedSession;

    const DAY: u64 = 86_400_000;

    #[test]
    fn sessions_group_by_local_calendar_day() {
        // 2026-11-02 10:00 UTC; a UTC+8 reader is at 18:00 that day.
        let now = 1_793_613_600_000;
        let offset = 8 * 3600;
        assert_eq!(DayGroup::of(now - 3_600_000, now, offset), DayGroup::Today);
        // 20 hours ago is 22:00 the previous local day.
        assert_eq!(
            DayGroup::of(now - 20 * 3_600_000, now, offset),
            DayGroup::Yesterday
        );
        assert_eq!(DayGroup::of(now - 3 * DAY, now, offset), DayGroup::ThisWeek);
        assert_eq!(DayGroup::of(now - 7 * DAY, now, offset), DayGroup::Earlier);
        assert_eq!(
            DayGroup::of(now + DAY, now, offset),
            DayGroup::Today,
            "a clock ahead"
        );
    }

    /// 320 sessions over 40 folders in 25 checkouts: deriving the projects,
    /// flattening the tree and windowing it -- the whole non-Blinc part of a
    /// sidebar rebuild -- stays far inside a frame, and builds a viewport's
    /// worth of rows, not 320.
    #[test]
    fn a_320_session_sidebar_lays_out_in_well_under_a_frame() {
        let sessions: Vec<ListedSession> = (0..320)
            .map(|i| ListedSession {
                asid: format!("ses_{i:04}"),
                directory: Some(format!("/home/u/w/repo{}/sub{}", i % 25, i % 40)),
            })
            .collect();
        let dirs: HashMap<String, DirInfo> = (0..320)
            .map(|i| {
                (
                    format!("/home/u/w/repo{}/sub{}", i % 25, i % 40),
                    DirInfo {
                        root: format!("/home/u/w/repo{}", i % 25).into(),
                        kind: ProjectKind::Git,
                        branch: Some("main".into()),
                        exists: true,
                    },
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let runs = 50;
        let mut built = 0;
        for _ in 0..runs {
            let projects = projects::derive(&projects::Sources {
                sessions: &sessions,
                dirs: &dirs,
                known: &[],
                recent: &[],
                prefs: &BTreeMap::new(),
                home: None,
            });
            let all: HashSet<String> = projects.iter().map(|p| p.id.clone()).collect();
            let open: HashMap<String, bool> = all.iter().map(|id| (id.clone(), true)).collect();
            let rows = projects::flatten(
                &projects,
                &TreeOptions {
                    query: "",
                    open: &open,
                    full: &all,
                    default_open: DEFAULT_OPEN,
                    per_project: PER_PROJECT,
                },
                |_| None,
            );
            let ordinary = projects.iter().filter(|p| !p.is_other()).count();
            assert!(
                ordinary <= projects::Project::MAX_PRIMARY,
                "ordinary projects capped; overflow in Other: {ordinary}"
            );
            let kept: usize = projects.iter().map(|p| p.sessions.len()).sum();
            assert_eq!(kept, 320, "every session still listed under some project");
            assert!(
                projects.iter().any(|p| p.is_other()),
                "overflow lands in Other"
            );
            let mut model = Model {
                heights: rows.iter().map(row_height).collect(),
                offsets: Vec::new(),
            };
            model.resum();
            let window = model.window(2000.0, 700.0, buffer(700.0).0);
            built = window.len();
        }
        let per = started.elapsed() / runs;
        eprintln!("320 sessions, everything open: {per:?} per layout, {built} rows built");
        assert!(built < 100, "a window, not the list: {built}");
        // Generous for a debug build on a loaded machine.
        assert!(per.as_millis() < 20, "{per:?}");
    }
}
