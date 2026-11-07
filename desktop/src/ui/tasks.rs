//! The tasks page: delegate a goal, watch, cancel or confirm tasks.

use rune::{col, row, text, El, ScrollSpec, TextEdit, Tx};
use rune_kit::{
    Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, Kit, ScrollBar, TextInput,
};

use super::{muted, UiState};
use crate::hub::TaskRow;
use crate::model::commands::Command;

fn create(s: &mut UiState) {
    let goal = s.task_goal.text.trim().to_string();
    if goal.is_empty() {
        return;
    }
    s.task_goal = TextEdit::new("");
    s.send(Command::CreateTask {
        goal,
        agent: None,
        cwd: None,
    });
}

fn state_badge(k: &Kit, state: &str) -> El {
    let variant = match state {
        "working" | "dispatching" => BadgeVariant::Accent,
        "done" => BadgeVariant::Neutral,
        _ => BadgeVariant::Subtle,
    };
    Badge::new(state.replace('_', " "))
        .variant(variant)
        .build(k)
}

fn card(k: &Kit, t: &TaskRow) -> El {
    let accent = match t.state.as_str() {
        "done" => k.c.success.icon,
        "failed" => k.c.danger.icon,
        "waiting" | "idle_unconfirmed" => k.c.warning.icon,
        "cancelled" => k.c.icon_faint,
        _ => k.c.info.icon,
    };
    let mut card = Card::new().accent(accent).title(t.title.clone(), None);
    let note = t
        .progress
        .clone()
        .or_else(|| t.summary.clone())
        .unwrap_or_default();
    if !note.is_empty() {
        card = card.description(note);
    }
    let mut meta = row().gap(6.0).center_y().child(state_badge(k, &t.state));
    if let Some(agent) = &t.agent_id {
        meta = meta.child(
            Badge::new(agent.clone())
                .variant(BadgeVariant::Neutral)
                .build(k),
        );
    }
    if t.children.total > 0 {
        meta = meta.child(muted(
            k,
            format!(
                "{} sub-task(s), {} active",
                t.children.total, t.children.active
            ),
        ));
    }
    card = card.child(meta);
    if let Some(asid) = t.asid.clone() {
        card = card.action(
            Button::new("Open session")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&asid))
                .build(k)
                .key(format!("task-open-{}", t.id)),
        );
    }
    if t.completable {
        let id = t.id.clone();
        card = card.action(
            Button::new("Mark done")
                .variant(ButtonVariant::Outline)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.send(Command::CompleteTask { id: id.clone() }))
                .build(k)
                .key(format!("task-done-{}", t.id)),
        );
    }
    if !t.is_ended() {
        let id = t.id.clone();
        card = card.action(
            Button::new("Cancel")
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.send(Command::CancelTask { id: id.clone() }))
                .build(k)
                .key(format!("task-cancel-{}", t.id)),
        );
    }
    card.build(k).key(format!("task-{}", t.id))
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let hint = match &s.task_cwd {
        Some(cwd) => format!("Runs in {cwd} unless the goal says otherwise."),
        None => "Runs in the gateway's default folder.".into(),
    };
    let mut page = col()
        .key("tasks")
        .w_full()
        .gap(12.0)
        .px(24.0)
        .py(24.0)
        .child(
            row()
                .key("task-new")
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    TextInput::new(&s.task_goal)
                        .placeholder("Delegate a goal…")
                        .label("Task goal")
                        .on_edit(|s: &mut UiState| &mut s.task_goal)
                        .on_submit(|s: &mut UiState, _: &mut Tx| create(s))
                        .build(k)
                        .key("task-goal")
                        .grow(1.0),
                )
                .child(
                    Button::new("Delegate")
                        .variant(ButtonVariant::Submit)
                        .control_size(ControlSize::Large)
                        .on_press(create)
                        .build(k)
                        .key("task-create"),
                )
                .child(
                    Button::new("Refresh")
                        .variant(ButtonVariant::Outline)
                        .control_size(ControlSize::Large)
                        .on_press(|s: &mut UiState| s.send(Command::RefreshTasks))
                        .build(k)
                        .key("task-refresh"),
                ),
        )
        .child(muted(k, hint));
    if s.tasks.is_empty() {
        page = page.child(text("No tasks yet.").font(k.ty.ui).color(k.c.text_muted));
    }
    for t in &s.tasks {
        page = page.child(card(k, t));
    }
    page.scroll(ScrollSpec {
        stick_bottom: false,
    })
    .scroll_bar(k)
}
