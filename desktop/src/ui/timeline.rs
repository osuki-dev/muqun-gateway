//! The active session's timeline.
//!
//! The scroll container is built once (with the root), bound to the drain's
//! `ScrollRef` for stick-to-bottom; only its content is reactive. The list
//! rebuilds when the order of rows changes; each row is its own keyed
//! `Stateful` that rebuilds when -- and only when -- that row's state is
//! written, so a streaming delta re-renders one row.
//!
//! [`render_part`] is the per-part renderer. It returns plain text for now;
//! streaming via `Text::bound` and markdown (task 0.3) replace its arms
//! without touching the list.

use std::sync::Arc;

use blinc_cn::prelude::{card, ButtonBuilder, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use muqun_gateway::agents::{
    AgentPart, AgentSessionId, PermissionDecision, PermissionRequest, TimelineItem, TimelineRole,
    ToolCallStatus,
};

use super::text::{mono_text, ui_text};
use super::App;
use crate::model::commands::Command;
use crate::model::view::item_state;

pub fn timeline(app: &App, height: f32) -> impl ElementBuilder {
    let order = app.bindings.order.clone();
    let permissions = app.bindings.permissions.clone();
    let selected = app.bindings.selected.clone();
    let list_app = app.clone();
    let list = stateful_with_key::<NoState>("mq-timeline-list")
        .deps([
            order.signal_id(),
            permissions.signal_id(),
            selected.signal_id(),
        ])
        .on_state(move |_| {
            let order = order.get();
            let mut list = div().class("mq-timeline-list").w_full().flex_col();
            if selected.get().is_none() {
                return list.child(
                    div()
                        .class("mq-empty")
                        .child(ui_text(list_app.strings.pick_session()).size(14.0)),
                );
            }
            if order.ids.is_empty() {
                list = list.child(
                    div()
                        .class("mq-empty")
                        .child(ui_text(list_app.strings.empty_timeline()).size(14.0)),
                );
            }
            let mut shown_approvals = Vec::new();
            for id in &order.ids {
                if let Some(item) = item_state(&order.asid, id).get() {
                    if let AgentPart::Approval { request } = &item.part {
                        shown_approvals.push(request.id.clone());
                    }
                }
                list = list.child(row(&list_app, &order.asid, id));
            }
            for request in permissions.get().iter() {
                if !shown_approvals.contains(&request.id) {
                    list = list.child(approval_card(&list_app, request));
                }
            }
            list
        });

    div().class("mq-timeline").w_full().h(height).child(
        scroll()
            .bind(&app.bindings.timeline_scroll)
            .w_full()
            .h(height)
            .child(list),
    )
}

fn row(app: &App, asid: &str, id: &str) -> impl ElementBuilder {
    let state = item_state(asid, id);
    let app = app.clone();
    stateful_with_key::<NoState>(format!("mq-row.{asid}.{id}"))
        .deps([state.signal_id()])
        .on_state(move |_| match state.get() {
            Some(item) => render_item(&app, &item),
            None => div(),
        })
}

fn render_item(app: &App, item: &Arc<TimelineItem>) -> Div {
    let mut row = div().class("mq-item").w_full().flex_col();
    if item.role == TimelineRole::User {
        row = row.class("mq-item--user");
    }
    row.child(render_part(app, item))
}

/// One part, as plain text. The arms are the seam 0.3 replaces.
pub fn render_part(app: &App, item: &TimelineItem) -> Div {
    match &item.part {
        AgentPart::Text { text } => div()
            .class("mq-part-text")
            .child(ui_text(text.clone()).size(14.0)),
        AgentPart::Reasoning { text, .. } => div()
            .class("mq-part-reasoning")
            .child(ui_text(format!("{} · {}", app.strings.thinking(), text.trim())).size(13.0)),
        AgentPart::Tool(call) => {
            let status = match call.status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::Streaming => "streaming",
                ToolCallStatus::Running => "running",
                ToolCallStatus::Completed => "done",
                ToolCallStatus::Failed => "failed",
            };
            let label = call.title.clone().unwrap_or_else(|| call.name.clone());
            div()
                .class("mq-part-tool")
                .flex_row()
                .items_center()
                .child(mono_text(call.name.clone()).size(12.0).no_wrap())
                .child(
                    div()
                        .class("mq-part-tool-title")
                        .flex_1()
                        .overflow_clip()
                        .child(
                            mono_text(if label == call.name {
                                String::new()
                            } else {
                                label
                            })
                            .size(12.0)
                            .no_wrap(),
                        ),
                )
                .child(ui_text(status).size(11.0).no_wrap())
        }
        AgentPart::Status { text } => div()
            .class("mq-part-status")
            .child(ui_text(text.clone()).size(12.0)),
        AgentPart::Approval { request } => approval_card(app, request),
        other => div()
            .class("mq-part-other")
            .child(ui_text(part_name(other)).size(12.0)),
    }
}

/// The part's wire name (`diff`, `todo`, `form` ...), for the placeholder row.
fn part_name(part: &AgentPart) -> String {
    serde_json::to_value(part)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "part".to_owned())
}

pub fn approval_card(app: &App, request: &PermissionRequest) -> Div {
    let reply = |label: &'static str, variant: ButtonVariant, decision: PermissionDecision| {
        let app = app.clone();
        let asid: AgentSessionId = request.asid.clone();
        let id = request.id.clone();
        ButtonBuilder::with_key(format!("mq-perm.{}.{}", id, decision.as_str()), label)
            .variant(variant)
            .size(ButtonSize::Small)
            .on_click(move |_| {
                app.send(Command::ReplyPermission {
                    asid: asid.clone(),
                    id: id.clone(),
                    decision,
                });
            })
    };
    let resources = request.resources.join("\n");
    div().class("mq-approval").w_full().child(
        card().w_full().child(
            div()
                .class("mq-approval-body")
                .flex_col()
                .child(
                    ui_text(app.strings.permission_needed())
                        .size(12.0)
                        .semibold(),
                )
                .child(ui_text(request.prompt.clone()).size(14.0))
                .child(mono_text(resources).size(12.0))
                .child(
                    div()
                        .class("mq-approval-actions")
                        .flex_row()
                        .child(reply(
                            app.strings.approve(),
                            ButtonVariant::Primary,
                            PermissionDecision::Allow,
                        ))
                        .child(reply(
                            app.strings.deny(),
                            ButtonVariant::Outline,
                            PermissionDecision::Deny,
                        )),
                ),
        ),
    )
}
