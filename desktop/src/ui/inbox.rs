//! The inbox page: every permission and form waiting on the owner, by session.

use rune::{col, row, text, El, ScrollSpec};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit, ScrollBar};

use super::{muted, timeline, title_of, UiState};
use crate::model::commands::Command;

pub fn view(s: &UiState, k: &Kit) -> El {
    let mut asids: Vec<&String> = s
        .waiting_perms
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(a, _)| a)
        .chain(
            s.waiting_forms
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(a, _)| a),
        )
        .collect();
    asids.sort();
    asids.dedup();
    let mut page = col().key("inbox").w_full().gap(16.0).px(24.0).py(24.0);
    for asid in asids {
        let title = s.session_by_id(asid).map_or_else(|| asid.clone(), title_of);
        let id = asid.clone();
        let mut block = col().key(format!("wait-{asid}")).w_full().gap(8.0).child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    text(title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .grow(1.0)
                        .truncate(),
                )
                .child(
                    Button::new("Open session")
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| s.open_session(&id))
                        .build(k)
                        .key(format!("open-{asid}")),
                ),
        );
        for p in s.waiting_perms.get(asid).into_iter().flatten() {
            block = block.child(timeline::permission_card(s, k, p).key(format!("p-{}", p.id)));
        }
        for f in s.waiting_forms.get(asid).into_iter().flatten() {
            block = block.child(timeline::form_card(s, k, f).key(format!("f-{}", f.id)));
        }
        page = page.child(block);
    }
    for t in s.tasks.iter().filter(|t| t.state == "waiting") {
        let mut line = row()
            .key(format!("task-{}", t.id))
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                col()
                    .grow(1.0)
                    .gap(2.0)
                    .child(
                        text(t.title.clone())
                            .font(k.ty.ui)
                            .color(k.c.text_base)
                            .truncate(),
                    )
                    .child(muted(
                        k,
                        format!("Task waiting · {}", t.progress.clone().unwrap_or_default()),
                    )),
            );
        if let Some(asid) = t.asid.clone() {
            line = line.child(
                Button::new("Open")
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| s.open_session(&asid))
                    .build(k)
                    .key(format!("task-open-{}", t.id)),
            );
        }
        if t.completable {
            let id = t.id.clone();
            line = line.child(
                Button::new("Mark done")
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Small)
                    .on_press(move |s: &mut UiState| {
                        s.send(Command::CompleteTask { id: id.clone() })
                    })
                    .build(k)
                    .key(format!("task-done-{}", t.id)),
            );
        }
        page = page.child(line);
    }
    if s.waiting_count() == 0 {
        page = page.child(muted(k, "Nothing is waiting for you."));
    }
    page.scroll(ScrollSpec {
        stick_bottom: false,
    })
    .scroll_bar(k)
}
