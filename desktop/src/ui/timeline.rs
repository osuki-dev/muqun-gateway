//! The active session's timeline: a windowed list on `scroll()`.
//!
//! Blinc's `virtual_list` builds its first 50 children and a spacer, with
//! no scroll tracking (architecture §8.2 R2), so the list is windowed here:
//! only rows within a viewport and a buffer of it are built; the rows above
//! and below are one spacer each, as tall as the model says they are
//! (`window.rs`). Each built row is a `Stateful` keyed by the item id that
//! renders again only when its own version signal is bumped (the drain does
//! that for changed items, `model/view.rs` `RowSignals`), and reports its
//! laid-out height so a row that leaves the window and comes back takes
//! exactly the room it had. The controller (`controller.rs`) decides the
//! window and keeps the viewport still: it never yanks the reader to new
//! content -- a "↓ N new messages" pill offers the jump instead -- and it
//! restores each session's position when the session is shown again.
//!
//! The streaming part renders through `Text::bound` (`parts/prose.rs`), so
//! a delta touches one text node; nothing on that path animates except the
//! cursor element's own keyframes.

pub mod controller;
pub mod qa;
pub mod window;
mod words;

use std::sync::Arc;

use blinc_layout::prelude::*;
use muqun_gateway::agents::{AgentPart, PermissionRequest};

use super::markdown::Palette;
use super::parts::{self, interact, scale, PartCtx};
use super::text::ui_text;
use super::App;
use crate::model::view::{segmented, split_row_id};
use controller::{Controller, LEAD};

/// The pending approvals the timeline shows as items of its own (the rest
/// are listed after the last row). Approval parts sit near the tail.
fn approvals_in_timeline(app: &App, pending: &[PermissionRequest]) -> Vec<String> {
    if pending.is_empty() {
        return Vec::new();
    }
    app.drain.with_view(|view| {
        let Some(active) = view.active.as_ref() else {
            return Vec::new();
        };
        active
            .items
            .iter()
            .rev()
            .take(400)
            .filter_map(|item| match &item.part {
                AgentPart::Approval { request } => Some(request.id.clone()),
                _ => None,
            })
            .collect()
    })
}

pub fn timeline(app: &App, height: f32) -> impl ElementBuilder {
    let ctl = Controller::get(app);
    let b = &app.bindings;
    let deps = [
        b.order.signal_id(),
        b.permissions.signal_id(),
        b.forms.signal_id(),
        b.selected.signal_id(),
        b.theme_epoch.signal_id(),
        ctl.window_epoch.signal_id(),
    ];
    let list_app = app.clone();
    let list_ctl = ctl.clone();
    let list = stateful_with_key::<NoState>("mq-timeline-list")
        .deps(deps)
        .on_state(move |_| build_list(&list_app, &list_ctl));

    let on_scroll = ctl.clone();
    let on_press = ctl.clone();
    div()
        .class("mq-timeline")
        .class(scale::Density::current().class())
        .relative()
        .w_full()
        .h(height)
        .child(
            Scroll::with_physics(ctl.physics())
                .bind(&app.bindings.timeline_scroll)
                // Rows outside the window are not built at all; culling
                // skips painting the parts of built rows (a long streaming
                // reply) that are off screen.
                .viewport_cull(std::env::var_os("MUQUN_QA_NOCULL").is_none())
                .w_full()
                .h(height)
                .on_scroll(move |_| on_scroll.scrolled())
                .on_mouse_down(move |_| on_press.scrolled())
                .child(list),
        )
        .child(pill(app, &ctl))
        .child(debug_overlay(&ctl))
}

/// `MUQUN_DEBUG_LIST=1`: the list's visible range, built window, total
/// rows and scroll offset, top right, updated each controller frame.
fn debug_overlay(ctl: &Arc<Controller>) -> Div {
    let Some(state) = ctl.debug.clone() else {
        return div();
    };
    div().absolute().left(16.0).top(8.0).child(
        stateful_with_key::<NoState>("mq-list-debug")
            .deps([state.signal_id()])
            .on_state(move |_| {
                div()
                    .class("mq-debug")
                    .child(parts::mono(state.get()).class("mq-text"))
            }),
    )
}

fn build_list(app: &App, ctl: &Arc<Controller>) -> Div {
    let started = std::time::Instant::now();
    let list = build_list_inner(app, ctl);
    tracing::debug!(
        target: "muqun::timeline",
        us = started.elapsed().as_micros() as u64,
        "list built"
    );
    list
}

fn build_list_inner(app: &App, ctl: &Arc<Controller>) -> Div {
    let b = &app.bindings;
    let mut list = div().class("mq-timeline-list").w_full().flex_col();
    let Some(selected) = b.selected.get() else {
        return list.child(
            div()
                .class("mq-empty")
                .child(ui_text(app.strings.pick_session()).size(scale::FS_13)),
        );
    };
    let order = b.order.get();
    if order.asid != selected {
        // The snapshot is on its way.
        return list;
    }
    let permissions = b.permissions.get();
    let forms = b.forms.get();
    if order.ids.is_empty() && permissions.is_empty() && forms.is_empty() {
        return list.child(teaching_empty_state(app));
    }
    let plan = ctl.plan(&order);
    let ids = ctl.ids(&plan.window);
    tracing::debug!(
        target: "muqun::timeline",
        rows = order.ids.len(),
        start = plan.window.start,
        end = plan.window.end,
        above = plan.above as u32,
        below = plan.below as u32,
        "window"
    );
    list = list
        .child(history_header(app, ctl, order.exhausted))
        .child(spacer(plan.above));
    let palette = Palette::current();
    let width = ctl
        .physics()
        .lock()
        .map(|p| p.viewport_width)
        .unwrap_or(800.0)
        .max(320.0)
        - 48.0;
    let agent = b
        .header
        .get()
        .map(|h| h.agent_id.clone())
        .unwrap_or_default();
    for id in ids {
        list = list.child(row(app, ctl, &order.asid, &id, palette, width, &agent));
    }
    list = list.child(spacer(plan.below));
    let shown = approvals_in_timeline(app, &permissions);
    for request in permissions.iter().filter(|r| !shown.contains(&r.id)) {
        list = list.child(
            div()
                .class("mq-row")
                .w_full()
                .child(interact::approval(app, request, None)),
        );
    }
    for form in forms.iter() {
        let sig = b.rows.signal(&order.asid, &format!("form:{}", form.id));
        let form_app = app.clone();
        let form = form.clone();
        list = list.child(
            stateful_with_key::<NoState>(format!("mq-pending-form.{}", form.id))
                .deps([sig.signal_id()])
                .on_state(move |_| {
                    div()
                        .class("mq-row")
                        .w_full()
                        .child(interact::form(&form_app, &form))
                }),
        );
    }
    list
}

fn spacer(height: f32) -> Div {
    div().w_full().h(height.max(0.0)).flex_shrink_0()
}

/// One row: its own `Stateful`, rebuilt only when its version signal is
/// bumped, reporting its height to the controller after each layout.
fn row(
    app: &App,
    ctl: &Arc<Controller>,
    asid: &str,
    id: &str,
    palette: Palette,
    width: f32,
    agent: &str,
) -> impl ElementBuilder {
    let sig = app.bindings.rows.signal(asid, id);
    let mut deps = vec![sig.signal_id()];
    let (item_id, _) = split_row_id(id);
    let item = app.drain.item(item_id);
    if let Some(AgentPart::Form { request }) = item.as_ref().map(|i| &i.part) {
        deps.push(
            app.bindings
                .rows
                .signal(asid, &format!("form:{}", request.id))
                .signal_id(),
        );
    }
    let app = app.clone();
    let measure = ctl.clone();
    let (asid, id, agent) = (asid.to_string(), id.to_string(), agent.to_string());
    let (m_asid, m_id) = (asid.clone(), id.clone());
    stateful_with_key::<NoState>(format!("mq-row.{asid}.{id}"))
        .deps(deps)
        .on_state(move |_| {
            let (item_id, k) = split_row_id(&id);
            let Some(item) = app.drain.item(item_id) else {
                return div().class("mq-row");
            };
            let (range, last) = match segmented(&item) {
                Some(text) => {
                    let segs = crate::model::stream::segments(text);
                    let n = segs.len();
                    (segs.get(k).cloned().unwrap_or(0..0), k + 1 >= n)
                }
                None => (0..usize::MAX, true),
            };
            let ctx = PartCtx {
                app: &app,
                asid: &asid,
                item: &item,
                live: app.drain.live_boundary(&asid, item_id),
                range,
                last,
                density: scale::Density::current(),
                palette,
                width,
                agent: &agent,
            };
            div().class("mq-row").w_full().child(parts::render(&ctx))
        })
        .on_layout(move |bounds| measure.measured(&m_asid, &m_id, bounds.height))
}

/// Fixed-height header above the first row: loading, the beginning of the
/// conversation, or a hint that there is more above.
fn history_header(app: &App, ctl: &Arc<Controller>, exhausted: bool) -> impl ElementBuilder {
    let loading = ctl.loading.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-history-header")
        .deps([loading.signal_id()])
        .on_state(move |_| {
            let label = if loading.get() {
                app.strings.loading_earlier()
            } else if exhausted {
                app.strings.start_of_history()
            } else {
                app.strings.earlier_available()
            };
            div()
                .class("mq-history")
                .w_full()
                .h(LEAD)
                .flex_row()
                .items_center()
                .justify_center()
                .child(parts::meta(label))
        })
}

/// A session with nothing in it teaches what to do (architecture §6.1d
/// item 2).
fn teaching_empty_state(app: &App) -> Div {
    let s = &app.strings;
    let mut lines = div().class("mq-teach-lines").flex_col();
    for line in [s.teach_send(), s.teach_stop(), s.teach_approve()] {
        lines = lines.child(
            ui_text(line)
                .size(scale::FS_13)
                .line_height(1.6)
                .class("mq-text-muted"),
        );
    }
    div()
        .class("mq-teach")
        .w_full()
        .flex_col()
        .items_center()
        .child(
            ui_text(s.teach_title())
                .size(scale::FS_18)
                .semibold()
                .class("mq-text"),
        )
        .child(lines)
}

/// "↓ N new messages", bottom right, while new rows wait below.
fn pill(app: &App, ctl: &Arc<Controller>) -> impl ElementBuilder {
    let count = ctl.pill.clone();
    let app = app.clone();
    let ctl = ctl.clone();
    stateful_with_key::<NoState>("mq-new-pill")
        .deps([count.signal_id()])
        .on_state(move |_| {
            let n = count.get();
            if n == 0 {
                return div();
            }
            let jump = ctl.clone();
            div().absolute().right(24.0).bottom(16.0).child(
                div()
                    .id("mq-new-pill")
                    .class("mq-pill")
                    .cursor_pointer()
                    .child(
                        ui_text(app.strings.new_messages(n))
                            .size(scale::FS_12)
                            .semibold()
                            .no_wrap()
                            .no_cursor(),
                    )
                    .on_click(move |_| jump.jump_to_bottom()),
            )
        })
}

/// Rows bake theme colours into their text: call after changing the colour
/// scheme so they render again (the drain also notices on its next run).
pub fn theme_changed(app: &App) {
    let epoch = app.bindings.theme_epoch.get();
    app.bindings.theme_epoch.signal().set(epoch + 1);
}
