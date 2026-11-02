//! One-line notices: status lines and the centred "switched" markers.

use blinc_layout::prelude::*;
use muqun_gateway::agents::ModelRef;
use muqun_gateway::home_relative;

use super::{meta, mono, PartCtx};

fn centred(children: Vec<Text>) -> Div {
    let mut line = div()
        .class("mq-notice")
        .w_full()
        .flex_row()
        .items_center()
        .justify_center();
    for child in children {
        line = line.child(child);
    }
    line
}

fn model_name(model: &ModelRef) -> String {
    match &model.variant {
        Some(v) => format!("{} ({v})", model.model_id),
        None => model.model_id.clone(),
    }
}

/// A muted status line.
pub fn status(_ctx: &PartCtx<'_>, text: &str) -> Div {
    div()
        .class("mq-status-line")
        .w_full()
        .child(meta(text.to_string()).line_height(1.5))
}

/// "Model · old → new".
pub fn model_switched(ctx: &PartCtx<'_>, model: &ModelRef, previous: Option<&ModelRef>) -> Div {
    let mut parts = vec![meta(ctx.app.strings.model_switched()), meta("·")];
    if let Some(prev) = previous {
        parts.push(mono(model_name(prev)));
        parts.push(meta("→"));
    }
    parts.push(mono(model_name(model)));
    centred(parts)
}

/// "Agent mode · plan → build".
pub fn agent_switched(ctx: &PartCtx<'_>, mode: &str, previous: Option<&str>) -> Div {
    let mut parts = vec![meta(ctx.app.strings.agent_switched()), meta("·")];
    if let Some(prev) = previous {
        parts.push(mono(prev.to_string()));
        parts.push(meta("→"));
    }
    parts.push(mono(mode.to_string()));
    centred(parts)
}

/// "Moved to · ~/work/x".
pub fn location_switched(ctx: &PartCtx<'_>, directory: &str, _previous: Option<&str>) -> Div {
    let home = dirs::home_dir();
    centred(vec![
        meta(ctx.app.strings.location_switched()),
        meta("·"),
        mono(home_relative(directory, home.as_deref())),
    ])
}
