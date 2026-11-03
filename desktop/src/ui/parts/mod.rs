//! One renderer per [`AgentPart`] variant, all taking a [`PartCtx`].
//!
//! Visual rules (architecture §6.1d): content is solid and high-contrast;
//! assistant prose sits flush with no bubble; the user's messages are a
//! quiet solid card; tool, diff and shell cards have a 1 px hairline and a
//! 3 px left accent and nothing else; paths, commands and models are
//! monospace. Nothing here animates: the only moving element on the
//! streaming path is the cursor (`styles.css` `mq-cursor`).
//!
//! Renderers are pure functions of the item and a little UI state (which
//! sections are open, form drafts). Changing that state bumps the row's
//! version signal ([`PartCtx::toggle`]), so only that row renders again.

pub mod cards;
pub mod interact;
pub mod notice;
pub mod prose;
pub mod scale;
pub mod summary;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use blinc_layout::prelude::*;
use muqun_gateway::agents::{AgentPart, TimelineItem, TimelineRole};

use super::markdown::{CodeChrome, Palette, Setting};
use super::text::{mono_text, ui_text};
use super::App;
use crate::model::turn::{RowTurn, TurnState};
use scale::Density;

/// Everything a part renderer needs.
pub struct PartCtx<'a> {
    pub app: &'a App,
    pub asid: &'a str,
    pub item: &'a TimelineItem,
    /// Where the settled prefix ends while this part streams (only on the
    /// row that holds the tail).
    pub live: Option<usize>,
    /// The bytes of the item's text this row shows (a long assistant text is
    /// several rows), and whether it is the item's last row.
    pub range: std::ops::Range<usize>,
    pub last: bool,
    pub density: Density,
    pub palette: Palette,
    /// The timeline's content width, for proportional limits.
    pub width: f32,
    /// The session's agent, for its brand accent.
    pub agent: &'a str,
    /// The turn this item is in (`model/turn.rs`).
    pub turn: Option<RowTurn>,
}

impl PartCtx<'_> {
    /// A prefix for this row's element ids. Ids only need to be unique
    /// within the window; item ids are unique within the session.
    pub fn key(&self, what: &str) -> String {
        format!("mq.{}.{}.{what}", self.asid, self.item.id)
    }

    /// The prose setting for this row.
    pub fn setting(&self, muted: bool) -> Setting {
        Setting {
            palette: self.palette,
            color: if muted {
                self.palette.muted
            } else {
                self.palette.text
            },
            size: scale::BODY,
            line_height: self.density.line_height(),
            width: self.width,
        }
    }

    pub fn chrome<'k>(&self, key: &'k str) -> CodeChrome<'k> {
        CodeChrome {
            key,
            copy_label: self.app.strings.copy(),
        }
    }

    /// Whether section `what` of this row is open (`default` until toggled).
    pub fn is_open(&self, what: &str, default: bool) -> bool {
        let toggled = open_sections()
            .lock()
            .unwrap()
            .contains(&self.section(what));
        toggled != default
    }

    fn section(&self, what: &str) -> String {
        format!("{}\u{0}{}\u{0}{what}", self.asid, self.item.id)
    }

    /// A click handler that flips section `what` and renders this row again.
    pub fn toggle(
        &self,
        what: &str,
    ) -> impl Fn(&blinc_layout::event_handler::EventContext) + 'static {
        let section = self.section(what);
        let rows = self.app.bindings.rows.clone();
        let asid = self.asid.to_string();
        let id = self.item.id.clone();
        move |_| {
            {
                let mut open = open_sections().lock().unwrap();
                if !open.remove(&section) {
                    open.insert(section.clone());
                }
            }
            for (state, version) in rows.bump(&asid, [&id]) {
                state.signal().set(version);
            }
        }
    }
}

/// Sections whose open state differs from their default, per row. Kept for
/// the process's life, so a session reopened later looks as it was left
/// (architecture §6.1c item 6).
fn open_sections() -> &'static Mutex<HashSet<String>> {
    static OPEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    OPEN.get_or_init(|| Mutex::new(HashSet::new()))
}

impl PartCtx<'_> {
    /// The turn's state; a row without one counts as finished.
    pub fn turn_state(&self) -> &TurnState {
        self.turn.as_ref().map_or(&TurnState::Done, |t| &t.state)
    }
}

/// The item's row: its part, and under the last item of a finished turn the
/// summary bar.
pub fn render(ctx: &PartCtx<'_>) -> Div {
    let content = render_part(ctx);
    let summary = ctx
        .turn
        .as_ref()
        .filter(|_| ctx.last)
        .and_then(|t| t.items.as_deref())
        .and_then(summary::summarize);
    match summary {
        Some(summary) => div()
            .w_full()
            .flex_col()
            .child(content)
            .child(summary::bar(ctx, &summary)),
        None => content,
    }
}

fn render_part(ctx: &PartCtx<'_>) -> Div {
    let item = ctx.item;
    match &item.part {
        AgentPart::Text { text } if item.role == TimelineRole::User => prose::user(ctx, text),
        AgentPart::Text { text } => prose::text(ctx, text),
        AgentPart::Reasoning { text, duration_ms } => prose::reasoning(ctx, text, *duration_ms),
        AgentPart::Tool(call) => cards::tool(ctx, call),
        AgentPart::Diff { file, diff } => cards::diff(ctx, file, diff),
        AgentPart::Todo { items } => cards::todo(ctx, items),
        AgentPart::Approval { request } => interact::approval(ctx.app, request, Some(&item.id)),
        AgentPart::Form { request } => interact::form(ctx.app, request),
        AgentPart::Status { text } => notice::status(ctx, text),
        AgentPart::Compaction {
            status,
            tokens,
            cost,
            error,
            ..
        } => cards::compaction(ctx, *status, tokens.as_ref(), *cost, error.as_ref()),
        AgentPart::Skill { skill, name, text } => cards::skill(ctx, skill, name, text),
        AgentPart::Shell {
            command,
            status,
            exit,
            output,
            truncated,
            ..
        } => cards::shell(ctx, command, status, *exit, output.as_deref(), *truncated),
        AgentPart::ModelSwitched { model, previous } => {
            notice::model_switched(ctx, model, previous.as_ref())
        }
        AgentPart::AgentSwitched { mode, previous } => {
            notice::agent_switched(ctx, mode, previous.as_deref())
        }
        AgentPart::LocationSwitched {
            directory,
            previous,
        } => notice::location_switched(ctx, directory, previous.as_deref()),
        AgentPart::Synthetic { text, description } => {
            prose::synthetic(ctx, text, description.as_deref())
        }
        AgentPart::System { text, description } => prose::system(ctx, text, description.as_deref()),
    }
}

/// A card: 1 px hairline, a 3 px left accent of `accent` (a class naming a
/// theme colour), then `body`.
pub fn card(accent: &str, body: Div) -> Div {
    div()
        .class("mq-card")
        .w_full()
        .flex_row()
        .items_stretch()
        .overflow_clip()
        .child(
            div()
                .class("mq-accent")
                .class(accent)
                .w(3.0)
                .flex_shrink_0(),
        )
        .child(body.class("mq-card-body").flex_1().min_w(0.0))
}

/// A small muted label.
pub fn meta(text: impl Into<String>) -> Text {
    ui_text(text).size(scale::META).no_wrap()
}

/// A monospace label that does not wrap (paths, models, commands).
pub fn mono(text: impl Into<String>) -> Text {
    mono_text(text).size(scale::MONO).no_wrap()
}

/// Monospace text that wraps (output, inputs).
pub fn mono_block(text: impl Into<String>) -> Text {
    mono_text(text).size(scale::FS_12).line_height(1.5)
}

/// At most `max` lines of `text`, and how many were left out.
pub fn clip_lines(text: &str, max: usize) -> (String, usize) {
    let total = text.lines().count();
    if total <= max {
        return (text.trim_end().to_string(), 0);
    }
    let kept: Vec<&str> = text.lines().take(max).collect();
    (kept.join("\n"), total - max)
}

/// A clickable muted text, for "Show output" and the like.
pub fn link_button(
    label: impl Into<String>,
    on_click: impl Fn(&blinc_layout::event_handler::EventContext) + 'static,
) -> Div {
    div()
        .class("mq-link-button")
        .cursor_pointer()
        .child(ui_text(label).size(scale::META).no_wrap().no_cursor())
        .on_click(on_click)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_counts_what_it_leaves_out() {
        assert_eq!(clip_lines("a\nb\nc\n", 5), ("a\nb\nc".to_string(), 0));
        assert_eq!(clip_lines("a\nb\nc\nd", 2), ("a\nb".to_string(), 2));
    }
}
