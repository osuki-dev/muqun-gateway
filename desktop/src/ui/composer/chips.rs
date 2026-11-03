//! What sits above the field: the prompts waiting behind a busy agent
//! ("Queued: …" with `×` and a "Next" toggle) and the staged files.

use std::sync::Arc;

use blinc_layout::prelude::*;

use muqun_gateway::agents::AgentSessionId;

use super::state::{Cmp, StagedStatus};
use super::Aim;
use crate::model::commands::Command;
use crate::model::compose::{ComposeCommand, InboxItem};
use crate::ui::glyphs::Glyph;
use crate::ui::icons;
use crate::ui::text::{mono_text, ui_text};
use crate::ui::App;

/// Queued prompts shown above the composer (legacy cap); the right panel
/// lists every item.
pub(super) const SHOWN: usize = 3;
/// A summary's longest line, in characters.
const SUMMARY_CHARS: usize = 64;
/// Staged image thumbnails in the chip row.
const THUMB: f32 = 28.0;

fn small_icon(glyph: Glyph) -> impl ElementBuilder {
    icons::tinted(glyph, 12.0)
}

/// The one line a waiting prompt shows: its first line, shortened.
pub fn summary(item: &InboxItem, attachments: &str) -> String {
    let first = item
        .text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let first = first.trim();
    if first.is_empty() {
        return if item.attachments > 0 {
            format!("{} {attachments}", item.attachments)
        } else {
            "…".to_string()
        };
    }
    if first.chars().count() > SUMMARY_CHARS {
        let cut: String = first.chars().take(SUMMARY_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        first.to_string()
    }
}

/// Full queue for the right-panel Queue tab (every item, scrollable).
pub fn queue_panel(app: &App) -> Option<Div> {
    let asid = AgentSessionId(app.bindings.selected.get()?);
    let items = Cmp::get().inbox.get();
    if items.is_empty() {
        return None;
    }
    Some(queue_list(app, &asid, &items, usize::MAX))
}

/// The waiting prompts of the open session, with what can be done to each.
#[allow(dead_code)]
pub fn waiting(app: &App, aim: &Aim) -> Option<Div> {
    let asid = aim.asid()?;
    let items = Cmp::get().inbox.get();
    if items.is_empty() {
        return None;
    }
    Some(queue_list(app, &asid, &items, SHOWN))
}

fn queue_list(app: &App, asid: &AgentSessionId, items: &[InboxItem], limit: usize) -> Div {
    let mut list = div().class("mq-queued-list").w_full().flex_col();
    for item in items.iter().take(limit) {
        let line = summary(item, app.strings.attachments_word());
        let label = if item.steer {
            app.strings.sending_next(&line)
        } else {
            app.strings.queued_label(&line)
        };
        let toggle = {
            let app = app.clone();
            let asid = asid.clone();
            let id = item.id.clone();
            let steer = item.steer;
            div()
                .class("mq-chip-toggle")
                .class(if steer {
                    "mq-chip-toggle--on"
                } else {
                    "mq-chip-toggle--off"
                })
                .cursor_pointer()
                .flex_shrink_0()
                .child(
                    ui_text(app.strings.next_label())
                        .size(11.0)
                        .semibold()
                        .no_wrap()
                        .no_cursor(),
                )
                .on_click(move |_| {
                    // Optimistic; `agent.inbox.changed` carries the whole
                    // queue and corrects it if the agent had already taken it.
                    let cmp = Cmp::get();
                    let mut next = (*cmp.inbox.get()).clone();
                    if let Some(entry) = next.iter_mut().find(|i| i.id == id) {
                        entry.steer = !steer;
                    }
                    cmp.inbox.signal().set(Arc::new(next));
                    app.send(Command::Compose(ComposeCommand::InboxDelivery {
                        asid: asid.clone(),
                        item: id.clone(),
                        steer: !steer,
                    }));
                })
        };
        // Edit returns the text to the draft (App parity). Attachments cannot
        // follow, so a prompt that carries them is only cancelled.
        let edit = (item.attachments == 0).then(|| {
            let app = app.clone();
            let asid = asid.clone();
            let id = item.id.clone();
            let text = item.text.clone();
            div()
                .class("mq-chip-x")
                .cursor_pointer()
                .flex_shrink_0()
                .child(small_icon(Glyph::Edit))
                .on_click(move |_| {
                    let cmp = Cmp::get();
                    let next: Vec<InboxItem> = cmp
                        .inbox
                        .get()
                        .iter()
                        .filter(|i| i.id != id)
                        .cloned()
                        .collect();
                    cmp.inbox.signal().set(Arc::new(next));
                    app.send(Command::Compose(ComposeCommand::InboxCancel {
                        asid: asid.clone(),
                        item: id.clone(),
                    }));
                    let end = text.chars().count();
                    super::set_text(&app, &text, end);
                    super::edited(&app);
                    super::focus_input(&app);
                })
        });
        let cancel = {
            let app = app.clone();
            let asid = asid.clone();
            let id = item.id.clone();
            div()
                .class("mq-chip-x")
                .cursor_pointer()
                .flex_shrink_0()
                .child(small_icon(Glyph::Close))
                .on_click(move |_| {
                    let cmp = Cmp::get();
                    let next: Vec<InboxItem> = cmp
                        .inbox
                        .get()
                        .iter()
                        .filter(|i| i.id != id)
                        .cloned()
                        .collect();
                    cmp.inbox.signal().set(Arc::new(next));
                    app.send(Command::Compose(ComposeCommand::InboxCancel {
                        asid: asid.clone(),
                        item: id.clone(),
                    }));
                })
        };
        let mut actions = div()
            .class("mq-queued-actions")
            .flex_row()
            .items_center()
            .child(toggle);
        if let Some(edit) = edit {
            actions = actions.child(edit);
        }
        actions = actions.child(cancel);
        list = list.child(
            div()
                .class("mq-queued")
                .w_full()
                .flex_row()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w(0.0)
                        .overflow_clip()
                        .child(ui_text(label).size(12.0).no_wrap().no_cursor()),
                )
                .child(actions),
        );
    }
    if limit < usize::MAX && items.len() > limit {
        list = list.child(
            div().class("mq-queued-more").child(
                ui_text(app.strings.more_queued(items.len() - limit))
                    .size(11.0)
                    .no_wrap()
                    .no_cursor(),
            ),
        );
    }
    list
}

/// The staged files, each with its state and a way to remove it.
pub fn staged(app: &App) -> Option<Div> {
    let cmp = Cmp::get();
    let staged = cmp.staged.get();
    if staged.is_empty() {
        return None;
    }
    let mut row = div().class("mq-chips").w_full().flex_row().flex_wrap();
    for entry in staged.iter() {
        let id = entry.id;
        let thumb = match &entry.status {
            StagedStatus::Ready(file) if file.image => Some(file.path.clone()),
            _ => None,
        };
        let (mark, class, note) = match &entry.status {
            StagedStatus::Uploading => (Glyph::File, "mq-chip--up", Some("…".to_string())),
            StagedStatus::Ready(file) => (
                if file.image {
                    Glyph::Photo
                } else {
                    Glyph::File
                },
                "mq-chip--ready",
                None,
            ),
            StagedStatus::Failed(why) => (Glyph::File, "mq-chip--failed", Some(why.clone())),
        };
        let mut name: String = entry.name.chars().take(28).collect();
        if entry.name.chars().count() > 28 {
            name.push('…');
        }
        let remove = {
            let app = app.clone();
            div()
                .class("mq-chip-x")
                .cursor_pointer()
                .flex_shrink_0()
                .child(small_icon(Glyph::Close))
                .on_click(move |_| {
                    let cmp = Cmp::get();
                    let next: Vec<_> = cmp
                        .staged
                        .get()
                        .iter()
                        .filter(|s| s.id != id)
                        .cloned()
                        .collect();
                    cmp.staged.signal().set(Arc::new(next));
                    let _ = &app;
                })
        };
        let mut chip = div()
            .class("mq-chip")
            .class(class)
            .class_when("mq-chip--thumb", thumb.is_some())
            .flex_row()
            .items_center();
        if let Some(path) = thumb {
            chip = chip.child(
                div()
                    .class("mq-chip-thumb")
                    .w(THUMB)
                    .h(THUMB)
                    .overflow_clip()
                    .flex_shrink_0()
                    .child(image(path).size(THUMB, THUMB).cover()),
            );
        } else {
            chip = chip.child(small_icon(mark));
        }
        chip = chip.child(mono_text(name).size(11.0).no_wrap().no_cursor());
        if let Some(note) = note {
            chip = chip.child(ui_text(note).size(11.0).no_wrap().no_cursor());
        }
        row = row.child(chip.child(remove));
    }
    Some(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(text: &str, attachments: usize) -> InboxItem {
        InboxItem {
            id: "i".into(),
            text: text.into(),
            steer: false,
            attachments,
        }
    }

    #[test]
    fn a_summary_is_the_first_line_shortened() {
        assert_eq!(summary(&item("hello\nworld", 0), "files"), "hello");
        assert_eq!(summary(&item("\n  \n", 2), "files"), "2 files");
        assert_eq!(summary(&item("", 0), "files"), "…");
        let long = "x".repeat(100);
        let s = summary(&item(&long, 0), "files");
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(s.ends_with('…'));
    }
}
