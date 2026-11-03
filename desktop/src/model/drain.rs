//! Main-thread drain: the one place core updates become Blinc state.
//!
//! # Mechanism (Blinc 8bfcc12, read from source)
//!
//! Two options were on the table; this module uses a **tick callback that
//! is registered only while there is work and removes itself once the inbox
//! is empty**. Why, with the evidence:
//!
//! * Draining at the top of the root `build_ui` does not work: the runner
//!   calls the UI builder only when the tree needs a full rebuild
//!   (`blinc_app/src/windowed.rs:6138` `if ws.needs_rebuild ||
//!   ws.render_tree.is_none()` → `invoke_ui_builder` at :6151). A woken frame
//!   with nothing structural to do never runs it, so the inbox would sit
//!   until the next resize or full rebuild -- and forcing a full rebuild per
//!   streamed token would rebuild every item.
//! * Tick callbacks run on the main thread, inside every rendered frame:
//!   the default `AnimationThreadMode::Main` (`blinc_platform/src/window.rs:45`)
//!   spawns no scheduler thread (`windowed.rs:2883-2892`), and the frame calls
//!   `rs.tick` (`windowed.rs:6459`) → `AnimationScheduler::tick`
//!   (`blinc_layout/src/render_state.rs:885`) → `step`, which runs every
//!   registered callback outside the scheduler lock
//!   (`blinc_animation/src/scheduler.rs:1001-1005`). So every `State` write
//!   below -- and every `Stateful` rebuild it triggers, which
//!   `check_stateful_deps` runs synchronously on the writing thread
//!   (`blinc_layout/src/stateful.rs:775-801`) -- happens on the main thread.
//!   The tokio side never writes Blinc state; it only pushes to the inbox.
//! * A registered callback keeps frames coming: `step` counts it as activity
//!   (`scheduler.rs:994` `|| !inner.tick_callbacks.is_empty()`) and re-arms
//!   `needs_redraw` (`scheduler.rs:1021-1023`). That is what lets a burst be
//!   drained frame by frame, and why the callback must not stay registered:
//!   removing it when the inbox is empty is what makes idle zero frames.
//! * Waking from tokio: `SchedulerHandle::register_tick_callback`
//!   (`scheduler.rs:1754-1769`) and `SchedulerHandle::request_redraw`
//!   (`scheduler.rs:1395-1398`) both fire the wake callback the runner
//!   installs before publishing the global handle (`windowed.rs:2872-2875`,
//!   `:2896-2900`), which marks the frame dirty and wakes the event loop out
//!   of `ControlFlow::Wait`. Before the window exists
//!   `try_get_scheduler()` (`scheduler.rs:81`) is `None` and pushes simply
//!   queue (architecture §8.2 R6).
//!
//! Writes go through each `State`'s underlying `Signal::set`, inside
//! `batch_stateful_deps`, so one batch notifies the dependent `Stateful`s
//! once (`blinc_core/src/reactive.rs:1496-1517`). (`State::set` calls the
//! context's callback directly and would not be batched, reactive.rs:1847.)
//!
//! # Streaming and the timeline
//!
//! The streaming item (`SessionView::live`) is not rebuilt per delta: its
//! row shows the settled markdown blocks plus a `Text::bound` tail
//! (`stream.rs`), and the drain writes only the tail, at most every
//! [`STREAM_INTERVAL`](super::stream::STREAM_INTERVAL). Its row rebuilds when
//! a block settles or the part stops streaming. Every other changed item
//! bumps its row's version signal, which exists only for rows the list has
//! built (`view::RowSignals`).
//!
//! The timeline view registers a [`FrameHook`]: each drained frame first
//! gives it the frame (the layout of everything published before is
//! current by then -- tick callbacks run after layout, `windowed.rs:6538`
//! vs `:6065`), then reports every batch to it before the batch is
//! published, so the list's window and scroll anchoring are decided before
//! the list rebuilds.
//!
//! Lock order, everywhere: `tick` → inbox → scheduler. The view lock is never
//! held while state is published: publishing runs dependent `Stateful`
//! builders synchronously, and rows read the view. The inbox's emptiness
//! is checked under the `tick` lock, and a push always takes the `tick` lock
//! (to arm) after its inbox lock is released, so a push can never land
//! between "the inbox is empty" and "the callback is gone".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use blinc_animation::TickCallbackId;
use blinc_theme::ThemeState;
use muqun_gateway::agents::{AgentPart, FormRequest, PermissionRequest, TimelineItem};

use super::inbox::{Inbox, UiAction, Update};
use super::stream::{self, TailKind, Throttle};
use super::view::{
    row_state, Bindings, Changes, SessionListing, SessionRow, TimelineOrder, ViewState,
};

pub type ActionHandler = Arc<dyn Fn(UiAction) + Send + Sync>;

/// What one batch did to the active timeline, for the timeline view.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineBatch {
    pub asid: String,
    /// A snapshot replaced the timeline (a newly opened session or a resync).
    pub opened: bool,
    pub appended: usize,
    pub prepended: usize,
    /// An older-history reply arrived.
    pub earlier: bool,
}

/// The timeline view's per-frame work (see the module docs).
pub trait FrameHook: Send + Sync {
    /// Called first in every drained frame. True while the hook wants more
    /// frames.
    fn frame(&self) -> bool;
    /// A batch changed the active timeline; called before it is published.
    fn batch(&self, batch: &TimelineBatch);
}

struct Tick {
    id: Option<TickCallbackId>,
}

/// The streaming item's split, as its row last rendered it.
#[derive(Default)]
struct Stream {
    /// (asid, item id) of the live item.
    key: Option<(String, String)>,
    boundary: usize,
    kind: Option<TailKind>,
    throttle: Throttle,
    /// The item changed since the tail was last written.
    dirty: bool,
    /// When settled blocks last moved into the row's markdown.
    last_settle: Option<Instant>,
}

/// What the sidebar was last given, so a batch writes only the rows -- and
/// the listing -- that differ. Main thread.
#[derive(Default)]
struct Published {
    listing: Option<Arc<SessionListing>>,
    rows: HashMap<String, SessionRow>,
}

struct Inner {
    inbox: Arc<Inbox>,
    tick: Mutex<Tick>,
    view: Mutex<ViewState>,
    published: Mutex<Published>,
    stream: Mutex<Stream>,
    bindings: Mutex<Option<Bindings>>,
    actions: Mutex<Option<ActionHandler>>,
    hook: Mutex<Option<Arc<dyn FrameHook>>>,
    /// The colours the rows were last built with: scheme and theme id.
    scheme: Mutex<Option<(blinc_theme::ColorScheme, Option<String>)>>,
}

#[derive(Clone)]
pub struct Drain {
    inner: Arc<Inner>,
}

/// The state writes one batch makes, computed under the view lock and
/// applied after it is released.
#[derive(Default)]
struct Plan {
    asid: String,
    /// Rows to render again.
    rows: Vec<String>,
    tail: Option<String>,
    order: Option<Arc<TimelineOrder>>,
    permissions: Option<Arc<Vec<PermissionRequest>>>,
    forms: Option<Arc<Vec<FormRequest>>>,
}

impl Drain {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        let drain = Self {
            inner: Arc::new(Inner {
                inbox: inbox.clone(),
                tick: Mutex::new(Tick { id: None }),
                view: Mutex::new(ViewState::default()),
                published: Mutex::new(Published::default()),
                stream: Mutex::new(Stream::default()),
                bindings: Mutex::new(None),
                actions: Mutex::new(None),
                hook: Mutex::new(None),
                scheme: Mutex::new(None),
            }),
        };
        let weak = Arc::downgrade(&drain.inner);
        inbox.set_wake(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                Drain { inner }.arm();
            }
            if let Some(scheduler) = blinc_animation::try_get_scheduler() {
                scheduler.request_redraw();
            }
        }));
        drain
    }

    /// Start draining into `bindings`. Called once, from the first
    /// `build_ui`, when Blinc's context and scheduler exist.
    pub fn attach(&self, bindings: Bindings, actions: ActionHandler) {
        *self.inner.bindings.lock().unwrap() = Some(bindings);
        *self.inner.actions.lock().unwrap() = Some(actions);
        self.arm();
    }

    /// Install the timeline view's frame hook. Main thread.
    pub fn set_frame_hook(&self, hook: Arc<dyn FrameHook>) {
        *self.inner.hook.lock().unwrap() = Some(hook);
    }

    /// Ask for frames: the hook is called each frame until it says it is
    /// done. Main thread.
    pub fn wake(&self) {
        self.arm();
        if let Some(scheduler) = blinc_animation::try_get_scheduler() {
            scheduler.request_redraw();
        }
    }

    /// Mark `asid` as the session the user picked. Main thread.
    pub fn select(&self, asid: &str) {
        let (changed, sidebar) = {
            let mut view = self.inner.view.lock().unwrap();
            let changed = view.select(asid);
            (changed, changed.then(|| self.plan_sidebar(&view)))
        };
        if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
            blinc_core::reactive::batch_stateful_deps(|| {
                bindings.draft.signal().set(None);
                bindings.settings_open.signal().set(false);
                bindings.inbox_open.signal().set(false);
                if changed {
                    bindings.selected.signal().set(Some(asid.to_string()));
                    bindings.diff.signal().set(None);
                    bindings.diff_panel.signal().set(false);
                    bindings.diff_focus.signal().set(None);
                }
                if let Some(sidebar) = sidebar {
                    sidebar.publish(&bindings);
                }
            });
        }
    }

    /// Back to Home. Main thread.
    pub fn go_home(&self) {
        let (changed, sidebar) = {
            let mut view = self.inner.view.lock().unwrap();
            let changed = view.deselect();
            (changed, changed.then(|| self.plan_sidebar(&view)))
        };
        if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
            blinc_core::reactive::batch_stateful_deps(|| {
                bindings.draft.signal().set(None);
                bindings.settings_open.signal().set(false);
                bindings.inbox_open.signal().set(false);
                if changed {
                    bindings.diff.signal().set(None);
                    bindings.diff_panel.signal().set(false);
                    bindings.diff_focus.signal().set(None);
                    bindings.selected.signal().set(None);
                    bindings.header.signal().set(None);
                    bindings.order.signal().set(Arc::new(Default::default()));
                }
                if let Some(sidebar) = sidebar {
                    sidebar.publish(&bindings);
                }
            });
        }
    }

    /// The sidebar listing if its layout moved, and each row whose content
    /// did, recorded as published. Main thread, under the view lock; the
    /// writes happen in [`SidebarPlan::publish`] after it is released.
    fn plan_sidebar(&self, view: &ViewState) -> SidebarPlan {
        let mut published = self.inner.published.lock().unwrap();
        let mut plan = SidebarPlan::default();
        let mut live = std::collections::HashSet::with_capacity(view.sessions.len());
        for row in view.rows() {
            live.insert(row.asid.clone());
            if published.rows.get(&row.asid) != Some(&row) {
                plan.rows.push(Arc::new(row.clone()));
                published.rows.insert(row.asid.clone(), row);
            }
        }
        published.rows.retain(|asid, _| live.contains(asid));
        let listing = view.listing();
        if published.listing.as_deref() != Some(&listing) {
            let listing = Arc::new(listing);
            published.listing = Some(listing.clone());
            plan.listing = Some(listing);
        }
        plan
    }

    pub fn dismiss_error(&self) {
        if self.inner.view.lock().unwrap().dismiss_error() {
            if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
                bindings.error.signal().set(None);
            }
        }
    }

    /// Read the view under its lock. Main thread; keep it short, and never
    /// write Blinc state inside.
    pub fn with_view<R>(&self, f: impl FnOnce(&ViewState) -> R) -> R {
        f(&self.inner.view.lock().unwrap())
    }

    /// One item of the active session.
    pub fn item(&self, id: &str) -> Option<Arc<TimelineItem>> {
        self.inner.view.lock().unwrap().item(id)
    }

    /// When `id` is the streaming item of `asid`: where its settled prefix
    /// ends, as its row should render it.
    pub fn live_boundary(&self, asid: &str, id: &str) -> Option<usize> {
        let stream = self.inner.stream.lock().unwrap();
        match &stream.key {
            Some((a, i)) if a == asid && i == id => Some(stream.boundary),
            _ => None,
        }
    }

    /// Queue UI-originated work for the next drain.
    pub fn post(&self, action: UiAction) {
        self.inner.inbox.push(Update::Ui(action));
    }

    /// Queue an update as if the core had sent it (the QA harness).
    pub fn inject(&self, update: Update) {
        self.inner.inbox.push(update);
    }

    /// Register the drain callback unless it already is.
    fn arm(&self) {
        let mut tick = self.inner.tick.lock().unwrap();
        if tick.id.is_some() || self.inner.bindings.lock().unwrap().is_none() {
            return;
        }
        let Some(scheduler) = blinc_animation::try_get_scheduler() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        tick.id = scheduler.register_tick_callback(move |_dt| {
            if let Some(inner) = inner.upgrade() {
                Drain { inner }.run();
            }
        });
    }

    /// One frame's drain. Runs on the main thread, inside the frame.
    fn run(&self) {
        let Some(bindings) = self.inner.bindings.lock().unwrap().clone() else {
            return;
        };
        let hook = self.inner.hook.lock().unwrap().clone();
        let hook_wants = hook.as_ref().is_some_and(|h| h.frame());
        self.check_scheme(&bindings);
        let batch = self.inner.inbox.take();
        if !batch.is_empty() {
            self.apply(&bindings, hook.as_deref(), batch);
        }
        let streaming = self.flush_stream(&bindings);
        let mut tick = self.inner.tick.lock().unwrap();
        if !hook_wants && !streaming && self.inner.inbox.is_empty() {
            if let (Some(id), Some(scheduler)) =
                (tick.id.take(), blinc_animation::try_get_scheduler())
            {
                scheduler.remove_tick_callback(id);
            }
        }
    }

    /// Rows bake theme colours into their rich text: a scheme or theme switch
    /// (from the top bar, Settings or the themes panel alike) has to render
    /// them again.
    fn check_scheme(&self, bindings: &Bindings) {
        let Some(theme) = ThemeState::try_get() else {
            return;
        };
        let colours = (theme.scheme(), crate::theme::apply::current_theme_id());
        let mut seen = self.inner.scheme.lock().unwrap();
        if seen.as_ref().is_some_and(|s| *s != colours) {
            bindings
                .theme_epoch
                .signal()
                .set(bindings.theme_epoch.get() + 1);
        }
        *seen = Some(colours);
    }

    fn apply(&self, bindings: &Bindings, hook: Option<&dyn FrameHook>, batch: Vec<Update>) {
        // Themes have their own bindings (`ui/themes.rs`); they never touch
        // the session view.
        let (themes, batch): (Vec<Update>, Vec<Update>) = batch
            .into_iter()
            .partition(|u| matches!(u, Update::Theme(_)));
        for update in themes {
            if let Update::Theme(update) = update {
                crate::ui::themes::apply_update(*update);
            }
        }
        // So do the composer's (`ui/composer/state.rs`).
        let (composer, batch): (Vec<Update>, Vec<Update>) = batch
            .into_iter()
            .partition(|u| matches!(u, Update::Composer(_)));
        for update in composer {
            if let Update::Composer(update) = update {
                crate::ui::composer::apply_update(*update);
            }
        }
        self.check_scheme(bindings);
        if batch.is_empty() {
            return;
        }
        let mut changes = Changes::default();
        let started = Instant::now();
        let kinds = tracing::enabled!(target: "muqun_desktop::drain", tracing::Level::DEBUG)
            .then(|| batch_kinds(&batch));
        let (plan, timeline_batch, applied, summary) = {
            let mut view = self.inner.view.lock().unwrap();
            for update in batch {
                view.apply(update, &mut changes);
            }
            let applied = started.elapsed();
            // Before `plan` takes the changed items.
            let summary = kinds.is_some().then(|| changes.summary());
            let touched = changes.timeline() || changes.earlier_reply;
            let sidebar = (changes.sessions || changes.selected || changes.attention)
                .then(|| self.plan_sidebar(&view));
            let plan = self.plan(&view, &mut changes);
            let timeline_batch = touched.then(|| TimelineBatch {
                asid: plan.asid.clone(),
                opened: changes.opened,
                appended: changes.appended,
                prepended: changes.prepended,
                earlier: changes.earlier_reply,
            });
            // The rest of the bindings, under the same lock, as values.
            let rest = Rest::from_view(&view, &changes);
            (
                Planned {
                    plan,
                    rest,
                    sidebar,
                },
                timeline_batch,
                applied,
                summary,
            )
        };
        if let (Some(hook), Some(batch)) = (hook, timeline_batch.as_ref()) {
            hook.batch(batch);
        }
        let rows = plan.plan.rows.len();
        blinc_core::reactive::batch_stateful_deps(|| plan.publish(bindings));
        if let (Some(kinds), Some(summary)) = (kinds, summary) {
            tracing::debug!(
                target: "muqun_desktop::drain",
                batch = %kinds,
                changed = %summary,
                rows,
                apply_us = applied.as_micros() as u64,
                publish_us = (started.elapsed() - applied).as_micros() as u64,
                "drained"
            );
        }
        if !changes.actions.is_empty() {
            let handler = self.inner.actions.lock().unwrap().clone();
            if let Some(handler) = handler {
                for action in changes.actions {
                    handler(action);
                }
            }
        }
    }

    /// Which rows to render again and what the streaming tail becomes.
    fn plan(&self, view: &ViewState, changes: &mut Changes) -> Plan {
        let mut plan = Plan::default();
        let Some(active) = view.active.as_ref() else {
            return plan;
        };
        plan.asid = active.info.asid.0.clone();
        let live = view.live().map(|item| (plan.asid.clone(), item.id.clone()));
        let mut stream = self.inner.stream.lock().unwrap();
        let mut rows: Vec<String> = std::mem::take(&mut changes.items);
        if stream.key != live || changes.opened {
            // The live item changed: the old one renders as markdown now,
            // the new one as settled blocks plus a tail.
            if let Some((asid, id)) = stream.key.take() {
                if asid == plan.asid && !rows.contains(&id) {
                    rows.push(id);
                }
            }
            *stream = Stream::default();
            tracing::debug!(
                target: "muqun::stream",
                live = live.as_ref().map(|(_, id)| id.as_str()).unwrap_or("-"),
                "live part changed"
            );
            if let Some((asid, id)) = live {
                if let Some(item) = view.item(&id) {
                    let text = part_text(&item);
                    stream.boundary = stream::settled_boundary(text);
                    let (kind, tail) = stream::tail(text, tail_from(text, stream.boundary));
                    stream.kind = Some(kind);
                    plan.tail = Some(tail.to_string());
                    stream.throttle.offer(Instant::now());
                }
                if !rows.contains(&id) {
                    rows.push(id.clone());
                }
                stream.key = Some((asid, id));
            }
        } else if let Some((_, id)) = live.as_ref() {
            // Same live item: its deltas go to the tail, throttled.
            if let Some(at) = rows.iter().position(|r| r == id) {
                rows.remove(at);
                stream.dirty = true;
            }
        }
        drop(stream);
        // Items to rows: a long text is several list rows.
        plan.rows = rows.iter().flat_map(|id| active.row_ids(id)).collect();
        if changes.order || changes.opened {
            plan.order = Some(Arc::new(view.order()));
        }
        if changes.permissions || changes.opened {
            plan.permissions = Some(Arc::new(active.pending_permissions.clone()));
        }
        if changes.forms || changes.opened {
            plan.forms = Some(Arc::new(active.pending_forms.clone()));
        }
        plan
    }

    /// Write the streaming item's tail when it changed and the throttle
    /// allows; render its row again when a block settled. True while an
    /// update is held back.
    fn flush_stream(&self, bindings: &Bindings) -> bool {
        let now = Instant::now();
        let (key, write) = {
            let mut stream = self.inner.stream.lock().unwrap();
            if !stream.dirty {
                return false;
            }
            let pass = if stream.throttle.is_pending() {
                stream.throttle.take_due(now)
            } else {
                stream.throttle.offer(now)
            };
            if !pass {
                return true;
            }
            stream.dirty = false;
            let Some((asid, id)) = stream.key.clone() else {
                return false;
            };
            let Some(item) = self.item(&id) else {
                return false;
            };
            let text = part_text(&item);
            let boundary = stream::settled_boundary(text);
            let moved = boundary != stream.boundary;
            let settled = moved && stream::may_settle(stream.last_settle, now);
            if settled {
                stream.boundary = boundary;
                stream.last_settle = Some(now);
            } else if moved {
                // Not yet: the new blocks stay in the tail until the next
                // settle, which a later frame retries.
                stream.dirty = true;
            }
            let (kind, tail) = stream::tail(text, tail_from(text, stream.boundary));
            let settled = settled || Some(kind) != stream.kind;
            stream.kind = Some(kind);
            // Only the last row of a segmented text holds the tail.
            let row = match stream::segments(text).len() {
                0 | 1 => id.clone(),
                n => super::view::row_id(&id, n - 1),
            };
            ((asid, row, settled), tail.to_string())
        };
        let (asid, id, settled) = key;
        let bumps = if settled {
            bindings.rows.bump(&asid, [&id])
        } else {
            Vec::new()
        };
        blinc_core::reactive::batch_stateful_deps(|| {
            bindings.stream_tail.signal().set(write);
            for (state, version) in bumps {
                state.signal().set(version);
            }
        });
        // A deferred settle keeps the drain ticking until it lands.
        self.inner.stream.lock().unwrap().dirty
    }
}

/// "event:agent.session.updated×3 agent_sessions×1", for the drain trace.
fn batch_kinds(batch: &[Update]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for update in batch {
        let kind = match update {
            Update::Agents(_) => "agents",
            Update::AgentSessions { .. } => "agent_sessions",
            Update::SessionCreated(_) => "created",
            Update::SessionGone(_) => "gone",
            Update::Snapshot(_) => "snapshot",
            Update::Event(event) => event.event_name(),
            Update::Error(_) => "error",
            Update::Projects { .. } => "projects",
            Update::Completions { .. } => "completions",
            Update::Ui(_) => "ui",
            Update::Earlier { .. } => "earlier",
            Update::Theme(_) => "theme",
            Update::Composer(_) => "composer",
            Update::Directories(_) => "directories",
            Update::KnownProjects { .. } => "known_projects",
            Update::Diff(_) => "diff",
        };
        match counts.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => counts.push((kind, 1)),
        }
    }
    counts
        .iter()
        .map(|(k, n)| format!("{k}×{n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where the tail starts: the settled boundary, or the start of the text's
/// last row when the boundary lags behind it (a settle held back by
/// [`stream::SETTLE_INTERVAL`] while a new row began).
pub fn tail_from(text: &str, boundary: usize) -> usize {
    let last = stream::segments(text).last().map_or(0, |r| r.start);
    boundary.max(last)
}

/// The text a streaming part shows.
fn part_text(item: &TimelineItem) -> &str {
    match &item.part {
        AgentPart::Text { text } | AgentPart::Reasoning { text, .. } => text,
        _ => "",
    }
}

/// The non-timeline bindings one batch changed, as values.
#[derive(Default)]
struct Rest {
    header: Option<Option<Arc<muqun_gateway::agents::AgentSessionInfo>>>,
    sessions: Option<Arc<Vec<muqun_gateway::agents::AgentSessionInfo>>>,
    selected: Option<Option<String>>,
    error: Option<Option<super::inbox::UiError>>,
    agents: Option<Arc<Vec<muqun_gateway::platform::discovery::AgentDiscoveryInfo>>>,
    attention: Option<Arc<Vec<PermissionRequest>>>,
    projects: Option<Arc<super::view::ProjectList>>,
    completions: Option<Arc<Vec<muqun_gateway::agents::directories::DirectoryEntry>>>,
    waiting_forms: Option<Arc<Vec<FormRequest>>>,
    dirs: Option<Arc<HashMap<String, super::projects::DirInfo>>>,
    known: Option<Arc<Vec<muqun_gateway::agents::AgentProject>>>,
    diff: Option<Option<super::view::DiffStat>>,
}

impl Rest {
    fn from_view(view: &ViewState, c: &Changes) -> Self {
        Self {
            header: c
                .header
                .then(|| view.active.as_ref().map(|a| Arc::new(a.info.clone()))),
            sessions: c.sessions.then(|| Arc::new(view.sessions.clone())),
            selected: c.selected.then(|| view.selected.clone()),
            error: c.error.then(|| view.error.clone()),
            agents: c.agents.then(|| Arc::new(view.agents.clone())),
            attention: c.attention.then(|| Arc::new(view.attention.clone())),
            projects: c.projects.then(|| Arc::new(view.projects.clone())),
            completions: c.completions.then(|| Arc::new(view.completions.clone())),
            waiting_forms: c.attention.then(|| Arc::new(view.waiting_forms.clone())),
            dirs: c.dirs.then(|| Arc::new(view.dirs.clone())),
            known: c
                .known
                .then(|| Arc::new(view.known.values().flatten().cloned().collect())),
            diff: (c.diff || c.selected).then(|| view.diff.clone()),
        }
    }
}

struct Planned {
    plan: Plan,
    rest: Rest,
    sidebar: Option<SidebarPlan>,
}

/// The sidebar writes one batch makes (see [`Drain::plan_sidebar`]).
#[derive(Default)]
struct SidebarPlan {
    rows: Vec<Arc<SessionRow>>,
    listing: Option<Arc<SessionListing>>,
}

impl SidebarPlan {
    /// Rows before the listing, so a list rebuilt for a new row finds its
    /// state already set. Inside a `batch_stateful_deps`.
    fn publish(self, b: &Bindings) {
        for row in self.rows {
            row_state(&row.asid).signal().set(Some(row));
        }
        if let Some(listing) = self.listing {
            b.listing.signal().set(listing);
        }
    }
}

impl Planned {
    /// Write the bindings. Rows and the tail are written before the order,
    /// so a list rebuilt for a new row finds every row's state already set.
    fn publish(self, b: &Bindings) {
        let Planned {
            plan,
            rest,
            sidebar,
        } = self;
        if let Some(sidebar) = sidebar {
            sidebar.publish(b);
        }
        if let Some(tail) = plan.tail {
            b.stream_tail.signal().set(tail);
        }
        for (state, version) in b.rows.bump(&plan.asid, &plan.rows) {
            state.signal().set(version);
        }
        if let Some(order) = plan.order {
            b.order.signal().set(order);
        }
        if let Some(permissions) = plan.permissions {
            b.permissions.signal().set(permissions);
        }
        if let Some(forms) = plan.forms {
            b.forms.signal().set(forms);
        }
        if let Some(header) = rest.header {
            b.header.signal().set(header);
        }
        if let Some(sessions) = rest.sessions {
            b.sessions.signal().set(sessions);
        }
        if let Some(selected) = rest.selected {
            b.selected.signal().set(selected);
        }
        if let Some(error) = rest.error {
            b.error.signal().set(error);
        }
        if let Some(agents) = rest.agents {
            b.agents.signal().set(agents);
        }
        if let Some(attention) = rest.attention {
            b.attention.signal().set(attention);
        }
        if let Some(projects) = rest.projects {
            b.projects.signal().set(projects);
        }
        if let Some(completions) = rest.completions {
            b.completions.signal().set(completions);
        }
        if let Some(forms) = rest.waiting_forms {
            b.waiting_forms.signal().set(forms);
        }
        if let Some(dirs) = rest.dirs {
            b.dirs.signal().set(dirs);
        }
        if let Some(known) = rest.known {
            b.known.signal().set(known);
        }
        if let Some(diff) = rest.diff {
            if b.diff.get() != diff {
                b.diff.signal().set(diff);
            }
        }
    }
}
