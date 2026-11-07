//! The tasks page: delegate a goal, watch, cancel or confirm tasks.

use rune::{row, El, TextEdit, Tx};
use rune_kit::{Badge, BadgeVariant, Button, ButtonVariant, Card, ControlSize, Kit, TextInput};

use super::copy::{tr, tr_ref};
use super::{muted, page_frame, UiState};
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

fn state_badge(k: &Kit, language: crate::config::Language, state: &str) -> El {
    let variant = match state {
        "working" | "dispatching" => BadgeVariant::Neutral,
        "done" => BadgeVariant::Neutral,
        _ => BadgeVariant::Subtle,
    };
    let label = match state {
        "working" => tr_ref(language, "working", "进行中"),
        "dispatching" => tr_ref(language, "dispatching", "派发中"),
        "queued" => tr_ref(language, "queued", "排队中"),
        "waiting" => tr_ref(language, "waiting", "等待中"),
        "done" => tr_ref(language, "done", "完成"),
        "failed" => tr_ref(language, "failed", "失败"),
        "cancelled" => tr_ref(language, "cancelled", "已取消"),
        "idle_unconfirmed" => tr_ref(language, "idle", "待确认"),
        other => other,
    };
    Badge::new(label.replace('_', " "))
        .variant(variant)
        .build(k)
}

fn card(s: &UiState, k: &Kit, t: &TaskRow) -> El {
    let language = s.cfg.general.language;
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
    let mut meta = row()
        .gap(6.0)
        .center_y()
        .child(state_badge(k, language, &t.state));
    if let Some(agent) = &t.agent_id {
        meta = meta.child(
            Badge::new(s.agent_name(agent))
                .variant(BadgeVariant::Neutral)
                .build(k),
        );
    }
    if t.children.total > 0 {
        meta = meta.child(muted(
            k,
            tr(
                language,
                &format!(
                    "{} sub-task(s), {} active",
                    t.children.total, t.children.active
                ),
                &format!(
                    "{} 个子任务，{} 个进行中",
                    t.children.total, t.children.active
                ),
            ),
        ));
    }
    card = card.child(meta);
    if let Some(asid) = t.asid.clone() {
        card = card.action(
            Button::new(tr_ref(language, "Open session", "打开会话"))
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
            Button::new(tr_ref(language, "Mark done", "标记完成"))
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
            Button::new(tr_ref(language, "Cancel", "取消"))
                .variant(ButtonVariant::Danger)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.send(Command::CancelTask { id: id.clone() }))
                .build(k)
                .key(format!("task-cancel-{}", t.id)),
        );
    }
    let look = s.look();
    look.row_card(
        k,
        card.build(k).px(6.0).py(4.0),
        crate::theme::look::CARD_RADIUS,
    )
    .key(format!("task-{}", t.id))
}

pub fn view(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let hint = match &s.task_cwd {
        Some(cwd) => tr(
            language,
            &format!("Runs in {cwd} unless the goal says otherwise."),
            &format!("默认在 {cwd} 运行，除非目标另有说明。"),
        ),
        None => tr(
            language,
            "Runs in the gateway's default folder.",
            "在 Gateway 默认文件夹中运行。",
        ),
    };
    let mut page = page_frame(
        k,
        "tasks",
        tr_ref(language, "Tasks", "任务"),
        Some(tr_ref(
            language,
            "Delegate a goal and watch it run.",
            "委派一个目标并跟踪进度。",
        )),
    )
    .child(
        row()
            .key("task-new")
            .w_full()
            .gap(8.0)
            .center_y()
            .child(
                TextInput::new(&s.task_goal)
                    .placeholder(tr_ref(language, "Delegate a goal…", "委派一个目标…"))
                    .label(tr_ref(language, "Task goal", "任务目标"))
                    .on_edit(|s: &mut UiState| &mut s.task_goal)
                    .on_submit(|s: &mut UiState, _: &mut Tx| create(s))
                    .build(k)
                    .key("task-goal")
                    .grow(1.0),
            )
            .child(
                Button::new(tr_ref(language, "Delegate", "委派"))
                    .variant(ButtonVariant::Submit)
                    .control_size(ControlSize::Large)
                    .on_press(create)
                    .build(k)
                    .key("task-create"),
            )
            .child(
                Button::new(tr_ref(language, "Refresh", "刷新"))
                    .variant(ButtonVariant::Outline)
                    .control_size(ControlSize::Large)
                    .on_press(|s: &mut UiState| s.send(Command::RefreshTasks))
                    .build(k)
                    .key("task-refresh"),
            ),
    )
    .child(muted(k, hint));
    if s.tasks.is_empty() {
        page = page.child(
            Card::new()
                .accent(k.c.icon_faint)
                .title(tr_ref(language, "No tasks yet", "还没有任务"), None)
                .description(tr(
                    language,
                    "Describe a goal above to start one.",
                    "在上方描述目标即可开始。",
                ))
                .build(k)
                .key("tasks-empty"),
        );
    }
    for t in &s.tasks {
        page = page.child(card(s, k, t));
    }
    super::page_stage(s, k, "tasks", page)
}
