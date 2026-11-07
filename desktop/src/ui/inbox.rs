//! The inbox page: every permission and form waiting on the owner, by session.

use rune::{col, row, text, El, ScrollSpec};
use rune_kit::{Button, ButtonVariant, Card, ControlSize, Kit, ScrollBar};

use super::copy::{tr, tr_ref};
use super::{muted, page_frame, timeline, title_of, UiState};
use crate::model::commands::Command;

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
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

    let mut page = page_frame(
        k,
        "inbox",
        tr_ref(language, "Inbox", "收件箱"),
        Some(tr_ref(
            language,
            "Approvals and forms waiting on you.",
            "等待你处理的审批与表单。",
        )),
    );

    for asid in &asids {
        let title = s
            .session_by_id(asid)
            .map_or_else(|| (*asid).clone(), title_of);
        let id = (*asid).clone();
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
                    Button::new(tr_ref(language, "Open session", "打开会话"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| s.open_session(&id))
                        .build(k)
                        .key(format!("open-{asid}")),
                ),
        );
        for p in s.waiting_perms.get(*asid).into_iter().flatten() {
            block = block.child(timeline::permission_card(s, k, p).key(format!("p-{}", p.id)));
        }
        for f in s.waiting_forms.get(*asid).into_iter().flatten() {
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
                        tr(
                            language,
                            &format!("Task waiting · {}", t.progress.clone().unwrap_or_default()),
                            &format!("任务等待中 · {}", t.progress.clone().unwrap_or_default()),
                        ),
                    )),
            );
        if let Some(asid) = t.asid.clone() {
            line = line.child(
                Button::new(tr_ref(language, "Open", "打开"))
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
                Button::new(tr_ref(language, "Mark done", "标记完成"))
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
    if s.waiting_count() == 0 && !s.tasks.iter().any(|t| t.state == "waiting") {
        page = page.child(
            Card::new()
                .accent(k.c.success.icon)
                .title(tr_ref(language, "All clear", "全部处理完毕"), None)
                .description(tr(
                    language,
                    "Nothing is waiting for you right now.",
                    "现在没有需要你处理的事项。",
                ))
                .build(k)
                .key("inbox-empty"),
        );
    }
    col()
        .key("inbox-scroll")
        .w_full()
        .h_full()
        .items_center()
        .child(page)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_bar(k)
}
