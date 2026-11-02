//! The timeline's scroll controller: which rows are built, where the
//! viewport is, and keeping it still while rows change (architecture §8.2
//! R2, §6.1c item 7).
//!
//! # Frame order (Blinc, read from source)
//!
//! A frame runs: scroll physics tick → pending `ScrollRef` commands →
//! subtree rebuilds → layout (row `on_layout` callbacks fire here,
//! `renderer/mod.rs:2314`) → tick callbacks (the drain, and through its
//! [`FrameHook`] this controller, `windowed.rs:6538`) → paint, which reads
//! the scroll offset live from the physics (`renderer/scroll.rs:771`). So
//! [`Controller::frame`] sees the layout of everything built so far and can
//! correct the offset before that layout is painted: a row that grew above
//! the viewport, an older page prepended, a window that materialised rows
//! whose estimate was wrong -- none of them moves what the reader sees.
//!
//! # Anchoring
//!
//! Each frame ends by remembering the anchor: the first row under the
//! viewport's top and that row's content y. The next frame computes the
//! same row's y from the model (which, after layout, matches the list row
//! for row, `window.rs`); any difference is content that moved under the
//! reader, and the offset moves with it. While following the tail the
//! offset is pinned to the bottom instead.
//!
//! The physics is owned here and handed to every `scroll()` built for the
//! timeline (`Scroll::with_physics`), so the offset survives the panel
//! being rebuilt and per-session positions can be restored.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use blinc_core::State;
use blinc_layout::prelude::*;
use muqun_gateway::agents::AgentSessionId;

use super::window::{Heights, Model};
use crate::model::commands::Command;
use crate::model::drain::{FrameHook, TimelineBatch};
use crate::model::view::{Shape, TimelineOrder};
use crate::ui::App;

/// Height of the history header above the first row (its padding
/// included); fixed, so loading/exhausted text never moves the rows.
pub const LEAD: f32 = 44.0;
/// Frames the controller keeps running after the last movement.
const SETTLE_FRAMES: u32 = 8;
/// After an older-history request failed, wait before asking again.
const RETRY_EARLIER: Duration = Duration::from_secs(3);
/// Viewport height before the first layout.
const DEFAULT_VIEWPORT: f32 = 900.0;
/// Remembered heights for this many sessions.
const KEEP_SESSIONS: usize = 24;

/// Where the reader was in a session.
#[derive(Debug, Clone, PartialEq)]
enum Saved {
    Bottom,
    /// The anchor row, and how far its top was below the viewport's top.
    Anchor {
        id: String,
        below_top: f32,
    },
}

#[derive(Debug, Clone)]
struct Restore {
    to: Saved,
    /// The list for this session has been built since: its layout is the
    /// one the next frame sees.
    built: bool,
}

#[derive(Default)]
struct Ctl {
    asid: String,
    ids: Vec<String>,
    shapes: Vec<Shape>,
    index: HashMap<String, usize>,
    heights: HashMap<String, Heights>,
    /// Sessions by last use, for evicting `heights`.
    recent: Vec<String>,
    model: Model,
    sums_dirty: bool,
    /// Content width the estimates use; changes rebuild the list.
    width: f32,
    window: Range<usize>,
    anchor: Option<(String, f32)>,
    following: bool,
    saved: HashMap<String, Saved>,
    restore: Option<Restore>,
    exhausted: bool,
    loading: bool,
    retry_at: Option<Instant>,
    pill: usize,
    quiet: u32,
    last_top: f32,
    last_content: f32,
    epoch: u64,
}

/// One window of the list, as the list builder renders it.
pub struct ListPlan {
    pub window: Range<usize>,
    pub above: f32,
    pub below: f32,
}

pub struct Controller {
    app: App,
    physics: SharedScrollPhysics,
    /// Bumped when the window changes; the list depends on it.
    pub window_epoch: State<u64>,
    /// New rows that arrived while the reader was away from the bottom.
    pub pill: State<usize>,
    /// An older page is on its way.
    pub loading: State<bool>,
    s: Mutex<Ctl>,
}

/// Signal writes decided under the lock, made after it is released (they
/// run dependent builders synchronously, which come back here).
#[derive(Default)]
struct Writes {
    epoch: Option<u64>,
    pill: Option<usize>,
    loading: Option<bool>,
    command: Option<Command>,
}

impl Controller {
    /// The controller, created on first use (main thread, Blinc up).
    pub fn get(app: &App) -> Arc<Controller> {
        static CONTROLLER: OnceLock<Arc<Controller>> = OnceLock::new();
        CONTROLLER
            .get_or_init(|| {
                let ctx = blinc_core::BlincContextState::get();
                let controller = Arc::new(Controller {
                    app: app.clone(),
                    physics: Arc::new(Mutex::new(ScrollPhysics::new(ScrollConfig::no_bounce()))),
                    window_epoch: ctx.use_state_keyed("mq.timeline.window", || 0u64),
                    pill: ctx.use_state_keyed("mq.timeline.pill", || 0usize),
                    loading: ctx.use_state_keyed("mq.timeline.loading", || false),
                    s: Mutex::new(Ctl {
                        width: 800.0,
                        ..Ctl::default()
                    }),
                });
                app.drain
                    .set_frame_hook(Arc::new(Hook(Arc::downgrade(&controller))));
                crate::ui::markdown::highlight::warm();
                controller
            })
            .clone()
    }

    pub fn physics(&self) -> SharedScrollPhysics {
        self.physics.clone()
    }

    /// (scroll top, viewport height, content height, viewport width).
    fn read(&self) -> (f32, f32, f32, f32) {
        let p = self.physics.lock().unwrap();
        let vh = if p.viewport_height > 0.0 {
            p.viewport_height
        } else {
            DEFAULT_VIEWPORT
        };
        (-p.offset_y, vh, p.content_height, p.viewport_width)
    }

    fn write_top(&self, top: f32) -> f32 {
        let mut p = self.physics.lock().unwrap();
        let max = (p.content_height - p.viewport_height).max(0.0);
        let top = top.clamp(0.0, max);
        p.offset_y = -top;
        p.velocity_y = 0.0;
        top
    }

    fn apply(&self, w: Writes) {
        if let Some(e) = w.epoch {
            self.window_epoch.signal().set(e);
        }
        if let Some(p) = w.pill {
            self.pill.signal().set(p);
        }
        if let Some(l) = w.loading {
            self.loading.signal().set(l);
        }
        if let Some(c) = w.command {
            self.app.send(c);
        }
    }

    /// Bring the model in line with `order`. Under the lock.
    fn sync(s: &mut Ctl, order: &TimelineOrder) {
        if order.asid != s.asid {
            s.asid = order.asid.clone();
        }
        s.exhausted = order.exhausted;
        if s.ids != order.ids {
            s.ids = order.ids.clone();
            s.shapes = order.shapes.clone();
            s.index = s
                .ids
                .iter()
                .enumerate()
                .map(|(i, id)| (id.clone(), i))
                .collect();
            let heights = s.heights.entry(s.asid.clone()).or_default();
            s.model = Model::build(&s.ids, &s.shapes, heights, s.width);
            s.sums_dirty = false;
        }
    }

    fn buffer(vh: f32) -> f32 {
        vh + 240.0
    }

    /// The window for the list being built now. Called from the list's
    /// builder: no signal writes here.
    pub fn plan(&self, order: &TimelineOrder) -> ListPlan {
        let (top, vh, _, _) = self.read();
        let mut guard = self.s.lock().unwrap();
        let s = &mut *guard;
        Self::sync(s, order);
        if s.sums_dirty {
            s.model.resum();
            s.sums_dirty = false;
        }
        let total = s.model.total();
        let bottom = (total - vh).max(0.0);
        let expected = match s.restore.as_mut() {
            Some(r) => {
                r.built = true;
                match &r.to {
                    Saved::Bottom => bottom,
                    Saved::Anchor { id, below_top } => s
                        .index
                        .get(id)
                        .map(|&i| s.model.top(i) - below_top)
                        .unwrap_or(bottom),
                }
            }
            None if s.following => bottom,
            None => {
                // Where the viewport will be once the anchor is honoured.
                let shift = s
                    .anchor
                    .as_ref()
                    .and_then(|(id, y)| s.index.get(id).map(|&i| s.model.top(i) - y))
                    .unwrap_or(0.0);
                top - LEAD + shift
            }
        };
        let window = if s.model.covers(&s.window, expected, vh, vh * 0.5) {
            s.window.clone()
        } else {
            s.model.window(expected, vh, Self::buffer(vh))
        };
        s.window = window.clone();
        let (above, below) = s.model.spacers(&window);
        ListPlan {
            window,
            above,
            below,
        }
    }

    pub fn ids(&self, window: &Range<usize>) -> Vec<String> {
        let s = self.s.lock().unwrap();
        s.ids
            .get(window.clone())
            .map(<[String]>::to_vec)
            .unwrap_or_default()
    }

    /// A row's laid-out height (its `on_layout`).
    pub fn measured(&self, asid: &str, id: &str, height: f32) {
        let changed = {
            let mut guard = self.s.lock().unwrap();
            let s = &mut *guard;
            if s.asid != asid {
                return;
            }
            let changed = s
                .heights
                .entry(asid.to_string())
                .or_default()
                .record(id, height);
            if changed {
                if let Some(&i) = s.index.get(id) {
                    s.model.set(i, height);
                    s.sums_dirty = true;
                }
                s.quiet = 0;
            }
            changed
        };
        if changed {
            self.app.drain.wake();
        }
    }

    /// A wheel or drag moved the viewport (event dispatch, before the
    /// frame): settle "following" now and widen the window if needed, so
    /// the rows are built before this frame's layout.
    pub fn scrolled(&self) {
        let (top, vh, content, _) = self.read();
        let writes = {
            let mut guard = self.s.lock().unwrap();
            let s = &mut *guard;
            s.quiet = 0;
            s.following = top >= (content - vh).max(0.0) - 2.0;
            s.restore = None;
            let mut w = Writes::default();
            if s.sums_dirty {
                s.model.resum();
                s.sums_dirty = false;
            }
            if !s.model.covers(&s.window, top - LEAD, vh, vh * 0.5) {
                s.window = s.model.window(top - LEAD, vh, Self::buffer(vh));
                s.epoch += 1;
                w.epoch = Some(s.epoch);
            }
            w
        };
        self.apply(writes);
        self.app.drain.wake();
    }

    /// The "↓ N new messages" pill was clicked.
    pub fn jump_to_bottom(&self) {
        let writes = {
            let mut guard = self.s.lock().unwrap();
            let s = &mut *guard;
            s.following = true;
            s.pill = 0;
            s.restore = Some(Restore {
                to: Saved::Bottom,
                built: false,
            });
            s.epoch += 1;
            Writes {
                epoch: Some(s.epoch),
                pill: Some(0),
                ..Writes::default()
            }
        };
        self.apply(writes);
        self.app.drain.wake();
    }

    fn batch(&self, batch: &TimelineBatch) {
        let writes = {
            let mut guard = self.s.lock().unwrap();
            let s = &mut *guard;
            let mut w = Writes::default();
            if batch.opened {
                if batch.asid != s.asid {
                    s.asid = batch.asid.clone();
                    s.ids.clear();
                    s.window = 0..0;
                    s.loading = false;
                    s.retry_at = None;
                    s.pill = 0;
                    w.pill = Some(0);
                    w.loading = Some(false);
                    s.recent.retain(|a| a != &batch.asid);
                    s.recent.push(batch.asid.clone());
                    if s.recent.len() > KEEP_SESSIONS {
                        let old = s.recent.remove(0);
                        s.heights.remove(&old);
                    }
                }
                s.anchor = None;
                s.following = false;
                let to = s.saved.get(&batch.asid).cloned().unwrap_or(Saved::Bottom);
                s.restore = Some(Restore { to, built: false });
            } else if batch.appended > 0 && !s.following {
                s.pill += batch.appended;
                w.pill = Some(s.pill);
            }
            if batch.earlier {
                s.loading = false;
                w.loading = Some(false);
                if batch.prepended == 0 {
                    s.retry_at = Some(Instant::now() + RETRY_EARLIER);
                }
            }
            s.quiet = 0;
            w
        };
        self.apply(writes);
    }

    /// The per-frame work; see the module docs. True while frames are
    /// still wanted.
    fn frame(&self) -> bool {
        let (mut top, vh, content, width) = self.read();
        let writes = {
            let mut guard = self.s.lock().unwrap();
            let s = &mut *guard;
            let mut w = Writes::default();
            if s.ids.is_empty() {
                s.quiet += 1;
                return s.quiet < SETTLE_FRAMES;
            }
            if s.sums_dirty {
                s.model.resum();
                s.sums_dirty = false;
            }
            let max = (content - vh).max(0.0);
            // 1. A session (re)opened: put the reader back.
            let mut restored = false;
            if let Some(r) = s.restore.clone().filter(|r| r.built) {
                s.restore = None;
                restored = true;
                match r.to {
                    Saved::Bottom => s.following = true,
                    Saved::Anchor { id, below_top } => {
                        if let Some(&i) = s.index.get(&id) {
                            top = self.write_top(LEAD + s.model.top(i) - below_top);
                        } else {
                            s.following = true;
                        }
                    }
                }
            }
            // 2. Content moved under the reader: move with it.
            if !restored && !s.following {
                if let Some((id, y)) = s.anchor.clone() {
                    if let Some(&i) = s.index.get(&id) {
                        let moved = s.model.top(i) - y;
                        if moved.abs() > 0.5 {
                            top = self.write_top(top + moved);
                        }
                    }
                }
            }
            // 3. Following the tail: stay on it.
            if s.following && (max - top).abs() > 0.5 {
                top = self.write_top(max);
            }
            s.following = top >= max - 2.0;
            if s.following && s.pill > 0 {
                s.pill = 0;
                w.pill = Some(0);
            }
            // 4. The new anchor.
            let i = s.model.row_at((top - LEAD).max(0.0));
            if let Some(id) = s.ids.get(i).cloned() {
                let y = s.model.top(i);
                s.saved.insert(
                    s.asid.clone(),
                    if s.following {
                        Saved::Bottom
                    } else {
                        Saved::Anchor {
                            id: id.clone(),
                            below_top: top - LEAD - y,
                        }
                    },
                );
                s.anchor = Some((id, y));
            }
            // 5. The window, for the next frame (or a width change: the
            //    estimates change, so do the spacers).
            let width_changed = width > 0.0 && (width - s.width).abs() > 1.0;
            if width_changed {
                s.width = width;
                let heights = s.heights.entry(s.asid.clone()).or_default();
                s.model = Model::build(&s.ids, &s.shapes, heights, width);
            }
            if width_changed || !s.model.covers(&s.window, top - LEAD, vh, vh * 0.5) {
                s.window = s.model.window(top - LEAD, vh, Self::buffer(vh));
                s.epoch += 1;
                w.epoch = Some(s.epoch);
            }
            // 6. Near the top: ask for the older page.
            let can_retry = s.retry_at.is_none_or(|at| Instant::now() >= at);
            if top - LEAD < vh && !s.exhausted && !s.loading && can_retry {
                s.loading = true;
                s.retry_at = None;
                w.loading = Some(true);
                w.command = Some(Command::LoadEarlier(AgentSessionId(s.asid.clone())));
            }
            tracing::trace!(
                target: "muqun::timeline",
                top,
                max,
                content,
                vh,
                following = s.following,
                start = s.window.start,
                end = s.window.end,
                "frame"
            );
            // 7. Keep going while anything moves.
            let moved = (top - s.last_top).abs() > 0.1
                || (content - s.last_content).abs() > 0.1
                || w.epoch.is_some();
            s.last_top = top;
            s.last_content = content;
            s.quiet = if moved { 0 } else { s.quiet + 1 };
            let wants = s.quiet < SETTLE_FRAMES || s.restore.is_some();
            (w, wants)
        };
        let (w, wants) = writes;
        self.apply(w);
        wants
    }

    /// The window currently built and the rows the viewport shows, for the
    /// QA harness's gap check.
    pub fn coverage(&self) -> (Range<usize>, Range<usize>) {
        let (top, vh, _, _) = self.read();
        let s = self.s.lock().unwrap();
        (
            s.window.clone(),
            s.model.rows_between(top - LEAD, top - LEAD + vh),
        )
    }

    pub fn row_count(&self) -> usize {
        self.s.lock().unwrap().ids.len()
    }
}

struct Hook(std::sync::Weak<Controller>);

impl FrameHook for Hook {
    fn frame(&self) -> bool {
        self.0.upgrade().is_some_and(|c| c.frame())
    }

    fn batch(&self, batch: &TimelineBatch) {
        if let Some(c) = self.0.upgrade() {
            c.batch(batch);
        }
    }
}
