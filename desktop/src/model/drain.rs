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
//! Lock order, everywhere: `tick` → inbox → scheduler. The inbox's emptiness
//! is checked under the `tick` lock, and a push always takes the `tick` lock
//! (to arm) after its inbox lock is released, so a push can never land
//! between "the inbox is empty" and "the callback is gone".

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use blinc_animation::TickCallbackId;

use super::inbox::{Inbox, UiAction, Update};
use super::view::{
    item_state, row_state, Bindings, Changes, SessionListing, SessionRow, ViewState,
};

/// Frames the timeline is re-pinned to the bottom after a batch that grew
/// it: `ScrollRef::scroll_to_bottom` resolves against the content height the
/// renderer knows at the start of the next frame
/// (`blinc_layout/src/renderer/scroll.rs:143-145`), which is before that
/// frame's subtree rebuild lays the new rows out.
const FOLLOW_FRAMES: u8 = 3;

pub type ActionHandler = Arc<dyn Fn(UiAction) + Send + Sync>;

struct Tick {
    id: Option<TickCallbackId>,
    follow: u8,
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
    bindings: Mutex<Option<Bindings>>,
    actions: Mutex<Option<ActionHandler>>,
}

#[derive(Clone)]
pub struct Drain {
    inner: Arc<Inner>,
}

impl Drain {
    pub fn new(inbox: Arc<Inbox>) -> Self {
        let drain = Self {
            inner: Arc::new(Inner {
                inbox: inbox.clone(),
                tick: Mutex::new(Tick {
                    id: None,
                    follow: 0,
                }),
                view: Mutex::new(ViewState::default()),
                published: Mutex::new(Published::default()),
                bindings: Mutex::new(None),
                actions: Mutex::new(None),
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

    /// Mark `asid` as the session the user picked. Main thread.
    pub fn select(&self, asid: &str) {
        let mut view = self.inner.view.lock().unwrap();
        let changed = view.select(asid);
        if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
            blinc_core::reactive::batch_stateful_deps(|| {
                bindings.draft.signal().set(None);
                bindings.settings_open.signal().set(false);
                if changed {
                    bindings.selected.signal().set(Some(asid.to_string()));
                    self.publish_sidebar(&view, &bindings);
                }
            });
        }
    }

    /// Back to Home. Main thread.
    pub fn go_home(&self) {
        let mut view = self.inner.view.lock().unwrap();
        let changed = view.deselect();
        if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
            blinc_core::reactive::batch_stateful_deps(|| {
                bindings.draft.signal().set(None);
                bindings.settings_open.signal().set(false);
                if changed {
                    bindings.selected.signal().set(None);
                    bindings.header.signal().set(None);
                    bindings.order.signal().set(Arc::new(Default::default()));
                    self.publish_sidebar(&view, &bindings);
                }
            });
        }
    }

    /// Write the sidebar listing if its layout moved, and each row whose
    /// content did. Main thread, inside a `batch_stateful_deps`.
    fn publish_sidebar(&self, view: &ViewState, bindings: &Bindings) {
        let mut published = self.inner.published.lock().unwrap();
        let mut live = std::collections::HashSet::with_capacity(view.sessions.len());
        for row in view.rows() {
            live.insert(row.asid.clone());
            if published.rows.get(&row.asid) != Some(&row) {
                row_state(&row.asid)
                    .signal()
                    .set(Some(Arc::new(row.clone())));
                published.rows.insert(row.asid.clone(), row);
            }
        }
        published.rows.retain(|asid, _| live.contains(asid));
        let listing = view.listing(crate::platform::shell::local_utc_offset());
        if published.listing.as_deref() != Some(&listing) {
            let listing = Arc::new(listing);
            published.listing = Some(listing.clone());
            bindings.listing.signal().set(listing);
        }
    }

    pub fn dismiss_error(&self) {
        if self.inner.view.lock().unwrap().dismiss_error() {
            if let Some(bindings) = self.inner.bindings.lock().unwrap().clone() {
                bindings.error.signal().set(None);
            }
        }
    }

    #[allow(dead_code)] // for views that read the whole state
    /// Read the view under its lock. Main thread; keep it short.
    pub fn with_view<R>(&self, f: impl FnOnce(&ViewState) -> R) -> R {
        f(&self.inner.view.lock().unwrap())
    }

    /// Queue UI-originated work for the next drain.
    pub fn post(&self, action: UiAction) {
        self.inner.inbox.push(Update::Ui(action));
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
        let batch = self.inner.inbox.take();
        if !batch.is_empty() {
            self.apply(&bindings, batch);
        }
        let follow = {
            let mut tick = self.inner.tick.lock().unwrap();
            let follow = tick.follow > 0;
            tick.follow = tick.follow.saturating_sub(1);
            if tick.follow == 0 && self.inner.inbox.is_empty() {
                if let (Some(id), Some(scheduler)) =
                    (tick.id.take(), blinc_animation::try_get_scheduler())
                {
                    scheduler.remove_tick_callback(id);
                }
            }
            follow
        };
        if follow {
            bindings.timeline_scroll.scroll_to_bottom();
        }
    }

    fn apply(&self, bindings: &Bindings, batch: Vec<Update>) {
        // Read before anything grows: was the reader following the tail?
        let was_at_bottom = bindings.timeline_scroll.is_at_bottom();
        let mut changes = Changes::default();
        let started = std::time::Instant::now();
        let kinds = tracing::enabled!(target: "muqun_desktop::drain", tracing::Level::DEBUG)
            .then(|| batch_kinds(&batch));
        {
            let mut view = self.inner.view.lock().unwrap();
            for update in batch {
                view.apply(update, &mut changes);
            }
            let applied = started.elapsed();
            blinc_core::reactive::batch_stateful_deps(|| {
                if changes.sessions || changes.selected || changes.attention {
                    self.publish_sidebar(&view, bindings);
                }
                publish(&view, bindings, &changes)
            });
            if let Some(kinds) = kinds {
                tracing::debug!(
                    target: "muqun_desktop::drain",
                    batch = %kinds,
                    changed = %changes.summary(),
                    apply_us = applied.as_micros() as u64,
                    publish_us = (started.elapsed() - applied).as_micros() as u64,
                    "drained"
                );
            }
        }
        if changes.timeline() && (was_at_bottom || changes.opened) {
            bindings.timeline_scroll.scroll_to_bottom();
            self.inner.tick.lock().unwrap().follow = FOLLOW_FRAMES;
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
}

/// "event:agent.session.updated×3 agent_sessions×1", for the drain trace.
fn batch_kinds(batch: &[Update]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for update in batch {
        let kind = match update {
            Update::Agents(_) => "agents",
            Update::AgentSessions { .. } => "agent_sessions",
            Update::SessionCreated(_) => "created",
            Update::Snapshot(_) => "snapshot",
            Update::Event(event) => event.event_name(),
            Update::Error(_) => "error",
            Update::Projects { .. } => "projects",
            Update::Completions { .. } => "completions",
            Update::Ui(_) => "ui",
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

/// Write the bindings `changes` names. Items are written before the order,
/// so a list rebuilt for a new row finds every row's state already set.
fn publish(view: &ViewState, b: &Bindings, changes: &Changes) {
    if let Some(active) = view.active.as_ref() {
        let asid = &active.info.asid.0;
        for id in &changes.items {
            if let Some(item) = view.item(id) {
                item_state(asid, id).signal().set(Some(item));
            }
        }
    }
    if changes.order || changes.opened {
        b.order.signal().set(Arc::new(view.order()));
    }
    if changes.permissions || changes.opened {
        let pending = view
            .active
            .as_ref()
            .map(|a| a.pending_permissions.clone())
            .unwrap_or_default();
        b.permissions.signal().set(Arc::new(pending));
    }
    if changes.header {
        b.header
            .signal()
            .set(view.active.as_ref().map(|a| Arc::new(a.info.clone())));
    }
    if changes.sessions {
        b.sessions.signal().set(Arc::new(view.sessions.clone()));
    }
    if changes.selected {
        b.selected.signal().set(view.selected.clone());
    }
    if changes.error {
        b.error.signal().set(view.error.clone());
    }
    if changes.agents {
        b.agents.signal().set(Arc::new(view.agents.clone()));
    }
    if changes.attention {
        b.attention.signal().set(Arc::new(view.attention.clone()));
    }
    if changes.projects {
        b.projects.signal().set(Arc::new(view.projects.clone()));
    }
    if changes.completions {
        b.completions
            .signal()
            .set(Arc::new(view.completions.clone()));
    }
}
