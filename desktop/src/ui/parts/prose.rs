//! Prose parts: assistant text, reasoning, the user's messages, and the
//! injected (synthetic / system) blocks.

use blinc_core::State;
use blinc_layout::binding::IntoReactive;
use blinc_layout::prelude::*;
use blinc_layout::text::Text;

use super::{media, meta, scale, PartCtx};
use crate::model::stream::{self, TailKind};
use crate::ui::markdown::{self, copy_button, Setting};
use crate::ui::text::{MONO_FONT, UI_FONT};

/// Markdown for this row's part of the text; on the streaming row, the
/// settled blocks, then the tail bound to `stream_tail`, then the cursor.
fn body(ctx: &PartCtx<'_>, text: &str, setting: &Setting, cursor: bool) -> Div {
    let key = ctx.key("md");
    let chrome = ctx.chrome(&key);
    let range = ctx.range.start.min(text.len())..ctx.range.end.min(text.len());
    let Some(boundary) = ctx.live.filter(|_| ctx.last) else {
        let own = text.get(range).unwrap_or("");
        return markdown::render(&markdown::parse(own), setting, &chrome);
    };
    let from = crate::model::drain::tail_from(text, boundary.min(text.len()));
    let settled = text.get(range.start..from.max(range.start)).unwrap_or("");
    let mut column = div().w_full().flex_col();
    if !settled.trim().is_empty() {
        column = column.child(markdown::render(
            &markdown::parse(settled),
            setting,
            &chrome,
        ));
    }
    let (kind, _) = stream::tail(text, from);
    let column = column.child(tail(&ctx.app.bindings.stream_tail, kind, setting));
    if !cursor {
        return column;
    }
    // Only while the turn is streaming (`TurnState::Streaming`), at the end
    // of the newest assistant text.
    column.child(
        div()
            .class("mq-cursor")
            .class("mq-agent-dot")
            .class(format!("mq-agent--{}", agent_class(ctx.agent))),
    )
}

/// The streaming tail: replaced in place on every (throttled) delta, never
/// rebuilt. Same face, size and line height as the settled prose, so
/// nothing moves when the part finishes and turns into markdown.
fn tail(state: &State<String>, kind: TailKind, setting: &Setting) -> Div {
    let bound = Text::bound(state.into_reactive());
    match kind {
        TailKind::Prose => div().class("mq-md-p").w(setting.width).child(
            bound
                .font(UI_FONT)
                .size(setting.size)
                .line_height(setting.line_height)
                .class(if setting.color == setting.palette.muted {
                    "mq-text-muted"
                } else {
                    "mq-text"
                }),
        ),
        TailKind::Code => div().class("mq-code").w_full().child(
            div().class("mq-code-body").w_full().overflow_clip().child(
                bound
                    .font(MONO_FONT)
                    .size(scale::FS_13)
                    .line_height(1.5)
                    .class("mq-text"),
            ),
        ),
    }
}

/// The agents the stylesheet has a brand colour for.
fn agent_class(agent: &str) -> &str {
    match agent {
        "codex" | "opencode" | "deepseek" | "t3" | "fx" | "qoder" | "pi" | "claude"
        | "antigravity" | "cursor" => agent,
        _ => "other",
    }
}

/// Assistant text: flush, no bubble, with a quiet copy control once done.
pub fn text(ctx: &PartCtx<'_>, text: &str) -> Div {
    let setting = ctx.setting(false);
    let mut row = div()
        .class("mq-assistant")
        .w_full()
        .flex_col()
        .child(body(ctx, text, &setting, true));
    if let Some(assets) = ctx.item.image_assets.as_ref().filter(|a| !a.is_empty()) {
        row = row.child(media::asset_row(ctx, assets));
    }
    if ctx.last && ctx.live.is_none() && !text.trim().is_empty() {
        let mut actions = div().class("mq-row-actions").flex_row().items_center();
        if let Some(muqun_gateway::agents::MessageGeneration::Message { tokens_per_second }) =
            &ctx.item.generation
        {
            if *tokens_per_second > 0.0 {
                actions = actions
                    .child(meta(format!("{tokens_per_second:.1} tok/s")).class("mq-text-subtle"));
            }
        }
        actions = actions.child(copy_button(
            &ctx.key("copy"),
            ctx.app.strings.copy(),
            text.to_string(),
        ));
        row = row.child(actions);
    }
    row
}

/// Reasoning: a one-line header, collapsed by default; the text (muted
/// markdown, or the live split) when opened.
pub fn reasoning(ctx: &PartCtx<'_>, text: &str, duration_ms: Option<u64>) -> Div {
    let strings = &ctx.app.strings;
    let open = ctx.is_open("reasoning", false);
    let label = match (ctx.live, duration_ms) {
        (Some(_), _) => strings.thinking_live().to_string(),
        (None, Some(ms)) => strings.thought_for(ms),
        (None, None) => strings.thinking().to_string(),
    };
    let header = div()
        .id(ctx.key("reasoning"))
        .class("mq-reasoning-head")
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(super::mono(if open { "▾" } else { "▸" }).no_cursor())
        .child(meta(label).no_cursor())
        .on_click(ctx.toggle("reasoning"));
    let mut row = div()
        .class("mq-reasoning")
        .w_full()
        .flex_col()
        .child(header);
    if open && !text.trim().is_empty() {
        row = row.child(div().class("mq-reasoning-body").w_full().child(body(
            ctx,
            text,
            &Setting {
                width: ctx.width - 13.0,
                ..ctx.setting(true)
            },
            false,
        )));
    }
    row
}

/// The user's message: a quiet solid card on the right.
pub fn user(ctx: &PartCtx<'_>, text: &str) -> Div {
    // ByPath / ACP agents bake uploads into the wire text; strip that block
    // for display and paint the paths as thumbs (same as the App).
    let (display, from_text) =
        muqun_gateway::agents::ports::agent::split_attachment_paths(text);
    let mut atts: Vec<String> = ctx
        .item
        .attachments
        .as_ref()
        .cloned()
        .unwrap_or_default();
    for path in from_text {
        if !atts.iter().any(|a| a == path) {
            atts.push(path.to_string());
        }
    }
    let display = display.trim_end();
    // The card is as wide as its longest line, up to 80 % of the column;
    // its text gets that width explicitly (see `markdown::Setting::width`).
    let max = (ctx.width * 0.8).max(240.0);
    let pad = 2.0 * 16.0 + 2.0;
    let options = blinc_layout::text_measure::TextLayoutOptions::new().with_font_name(UI_FONT);
    let widest = display
        .lines()
        .take(200)
        .map(|l| {
            blinc_layout::text_measure::measure_text_with_options(l, scale::BODY, &options).width
        })
        .fold(0.0f32, f32::max);
    // Keep room for a strip of thumbs when the prose is short or empty.
    let media_floor = match atts.len() {
        0 => 0.0,
        1 => 200.0,
        n => ((n as f32) * 120.0).min(max - pad),
    };
    let inner = (widest.max(media_floor) + 2.0).clamp(120.0, max - pad);
    let setting = Setting {
        width: inner,
        ..ctx.setting(false)
    };
    let mut card = div().class("mq-user").w(inner + pad).flex_col();
    if !display.is_empty() {
        let paragraph = markdown::Inline {
            text: display.to_string(),
            spans: Vec::new(),
        };
        card = card.child(
            markdown::inline_text(&paragraph, &setting, setting.size, false).w(inner),
        );
    }
    if !atts.is_empty() {
        card = card.child(media::attachment_row(
            ctx,
            &atts,
            ctx.item.image_assets.as_deref(),
        ));
    } else if let Some(assets) = ctx.item.image_assets.as_ref().filter(|a| !a.is_empty()) {
        card = card.child(media::asset_row(ctx, assets));
    }
    card = card.child(
        div()
            .class("mq-user-foot")
            .flex_row()
            .items_center()
            .child(copy_button(
                &ctx.key("copy"),
                ctx.app.strings.copy(),
                if display.is_empty() {
                    text.to_string()
                } else {
                    display.to_string()
                },
            )),
    );
    div()
        .class("mq-user-row")
        .w_full()
        .flex_row()
        .justify_end()
        .child(card)
}

fn injected(ctx: &PartCtx<'_>, label: &str, text: &str, description: Option<&str>) -> Div {
    let open = ctx.is_open("injected", false);
    let mut head = div()
        .id(ctx.key("injected"))
        .class("mq-injected-head")
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(super::mono(if open { "▾" } else { "▸" }).no_cursor())
        .child(meta(label.to_string()).no_cursor());
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        head = head.child(meta(d.to_string()).no_cursor());
    }
    let mut block = div()
        .class("mq-injected")
        .w_full()
        .flex_col()
        .child(head.on_click(ctx.toggle("injected")));
    if open {
        let (shown, more) = super::clip_lines(text, 200);
        block = block.child(
            div()
                .class("mq-injected-body")
                .w_full()
                .child(markdown::render(
                    &markdown::parse(&shown),
                    &Setting {
                        width: ctx.width - 13.0,
                        ..ctx.setting(true)
                    },
                    &ctx.chrome(&ctx.key("md")),
                )),
        );
        if more > 0 {
            block = block.child(meta(ctx.app.strings.more_lines(more)));
        }
    }
    block
}

/// Text the agent's harness injected into the history.
pub fn synthetic(ctx: &PartCtx<'_>, text: &str, description: Option<&str>) -> Div {
    injected(ctx, ctx.app.strings.synthetic(), text, description)
}

/// A system message.
pub fn system(ctx: &PartCtx<'_>, text: &str, description: Option<&str>) -> Div {
    injected(ctx, ctx.app.strings.system(), text, description)
}

#[cfg(test)]
mod tests {
    use super::agent_class;

    #[test]
    fn unknown_agents_get_the_neutral_accent() {
        assert_eq!(agent_class("codex"), "codex");
        assert_eq!(agent_class("mystery"), "other");
    }
}
