//! The Tasks page (sidebar → Tasks): work delegated to agents through the
//! gateway's task ledger (`/api/tasks`). A card to delegate a new goal, then
//! one card per task with its state, what it is doing, and the actions that
//! apply (open its session, cancel, confirm finished).
//!
//! The list is refreshed when the page opens and by its refresh button; the
//! desktop does not follow the gateway's task frames yet.

use blinc_cn::prelude::{button, ButtonSize, ButtonVariant};
use blinc_layout::prelude::*;
use muqun_gateway::agents::AgentSessionId;

use super::glyphs::Glyph;
use super::icons;
use super::settings::{clear_text, hint, section_card, text_data, text_field, tr};
use super::text::ui_text;
use super::App;
use crate::hub::TaskRow;
use crate::model::commands::Command;

const PAGE_WIDTH: f32 = 720.0;
/// The header, which the scrolling body's height leaves room for.
const CHROME: f32 = 64.0;

const GOAL_KEY: &str = "mq-tasks.goal";
const AGENT_KEY: &str = "mq-tasks.agent";
const CWD_KEY: &str = "mq-tasks.cwd";

/// Open the page and read the list.
pub fn open(app: &App) {
    app.bindings.draft.signal().set(None);
    app.bindings.settings_open.signal().set(false);
    app.bindings.inbox_open.signal().set(false);
    app.bindings.tasks_open.signal().set(true);
    app.send(Command::RefreshTasks);
}

pub fn close(app: &App) {
    app.bindings.tasks_open.signal().set(false);
}

/// A task state in words.
fn state_word(app: &App, state: &str) -> String {
    match state {
        "queued" => tr(app, "Queued", "排队中"),
        "dispatching" => tr(app, "Starting", "启动中"),
        "working" => tr(app, "Working", "进行中"),
        "waiting" => tr(app, "Waiting for you", "等你处理"),
        "idle_unconfirmed" => tr(app, "Idle, not confirmed", "已空闲，待确认"),
        "done" => tr(app, "Done", "已完成"),
        "failed" => tr(app, "Failed", "失败"),
        "cancelled" => tr(app, "Cancelled", "已取消"),
        other => return other.to_string(),
    }
    .to_string()
}

/// The colour class of a state's word (the settings page's standing colours).
fn state_class(state: &str) -> &'static str {
    match state {
        "done" => "mq-standing--usable",
        "queued" | "dispatching" | "working" => "mq-standing--checking",
        _ => "mq-standing--blocked",
    }
}

pub fn page(app: &App, height: f32) -> Div {
    let body_height = (height - CHROME).max(120.0);
    let close_app = app.clone();
    let refresh_app = app.clone();
    let icon_button = |glyph: Glyph| {
        div()
            .class("mq-icon-button")
            .w(28.0)
            .h(28.0)
            .flex_row()
            .items_center()
            .justify_center()
            .flex_shrink_0()
            .cursor_pointer()
            .child(icons::tinted(glyph, 16.0))
    };
    div()
        .class("mq-settings")
        .w_full()
        .h(height)
        .flex_col()
        .child(
            div()
                .class("mq-settings-head")
                .w_full()
                .flex_row()
                .items_center()
                .child(
                    div().class("mq-settings-title").flex_1().child(
                        ui_text(tr(app, "Tasks", "任务"))
                            .size(18.0)
                            .semibold()
                            .no_wrap(),
                    ),
                )
                .child(
                    icon_button(Glyph::Refresh)
                        .on_click(move |_| refresh_app.send(Command::RefreshTasks)),
                )
                .child(icon_button(Glyph::Close).on_click(move |_| close(&close_app))),
        )
        .child(
            div().w_full().h(body_height).child(
                scroll().w_full().h(body_height).child(
                    div().w_full().flex_row().justify_center().child(
                        div()
                            .class("mq-settings-pane")
                            .w_full()
                            .max_w(PAGE_WIDTH)
                            .flex_col()
                            .class("mq-set-col")
                            .child(create_card(app))
                            .child(list(app)),
                    ),
                ),
            ),
        )
}

/// Goal, optional agent and folder, and the button that delegates them.
fn create_card(app: &App) -> Div {
    let b = app.bindings.clone();
    let goal = text_data(GOAL_KEY, "");
    let agent = text_data(AGENT_KEY, "");
    let cwd = text_data(CWD_KEY, "");
    let goal_field = text_field(&goal)
        .label(tr(app, "Delegate a task", "委派任务"))
        .placeholder(tr(app, "What should the agent do?", "让 agent 做什么？"))
        .w_full();
    let agent_field = text_field(&agent)
        .label(tr(app, "Agent (optional)", "Agent（可选）"))
        .placeholder(tr(app, "the gateway's primary agent", "网关的默认 agent"))
        .w_full();
    let cwd_hint = b
        .task_cwd
        .get()
        .unwrap_or_else(|| tr(app, "your home folder", "主目录").to_string());
    let cwd_field = text_field(&cwd)
        .label(tr(app, "Folder (optional)", "文件夹（可选）"))
        .placeholder(cwd_hint)
        .w_full();
    let submit = {
        let app = app.clone();
        let (goal, agent, cwd) = (goal.clone(), agent.clone(), cwd.clone());
        button(tr(&app, "Delegate", "委派"))
            .variant(ButtonVariant::Primary)
            .size(ButtonSize::Small)
            .on_click(move |_| {
                let read = |data: &blinc_layout::widgets::text_input::SharedTextInputData| {
                    data.lock()
                        .map(|d| d.value.trim().to_string())
                        .unwrap_or_default()
                };
                let text = read(&goal);
                if text.is_empty() {
                    return;
                }
                let some = |value: String| Some(value).filter(|v| !v.is_empty());
                app.send(Command::CreateTask {
                    goal: text,
                    agent: some(read(&agent)),
                    cwd: some(read(&cwd)),
                });
                // The goal is spent; the agent and folder are kept for the next.
                clear_text(GOAL_KEY);
            })
    };
    section_card(
        None,
        div()
            .w_full()
            .flex_col()
            .class("mq-set-col")
            .child(goal_field)
            .child(agent_field)
            .child(cwd_field)
            .child(
                div()
                    .class("mq-set-actions")
                    .w_full()
                    .flex_row()
                    .child(submit),
            )
            .child(hint(tr(
                app,
                "Runs through the Muqun gateway on this computer; results appear below.",
                "通过本机的 Muqun 网关运行，结果会显示在下方。",
            ))),
    )
}

/// One card per task, rebuilt when the list changes.
fn list(app: &App) -> impl ElementBuilder {
    let tasks = app.bindings.tasks.clone();
    let cwd = app.bindings.task_cwd.clone();
    let app = app.clone();
    stateful_with_key::<NoState>("mq-tasks-list")
        .deps([tasks.signal_id(), cwd.signal_id()])
        .on_state(move |_| {
            let rows = tasks.get();
            let mut col = div().w_full().flex_col().class("mq-set-col");
            if rows.is_empty() {
                return col.child(div().class("mq-empty").child(
                    ui_text(tr(&app, "No delegated tasks yet.", "还没有委派的任务。")).size(13.0),
                ));
            }
            for row in rows.iter() {
                col = col.child(task_card(&app, row));
            }
            col
        })
}

fn task_card(app: &App, row: &TaskRow) -> Div {
    let title = if row.goal.trim().is_empty() {
        row.title.clone()
    } else {
        first_line(&row.goal)
    };
    let mut head = div()
        .w_full()
        .flex_row()
        .items_center()
        .class("mq-agent-section-head")
        .child(
            div()
                .class("mq-agent-section-name")
                .flex_1()
                .min_w(0.0)
                .child(ui_text(title).size(13.0).semibold().no_wrap()),
        )
        .child(
            div()
                .class("mq-standing")
                .class(state_class(&row.state))
                .flex_shrink_0()
                .child(ui_text(state_word(app, &row.state)).size(11.0).no_wrap()),
        );
    if let Some(asid) = row.asid.clone() {
        let click = app.clone();
        head = head.child(
            button(tr(app, "Open", "打开"))
                .variant(ButtonVariant::Outline)
                .size(ButtonSize::Small)
                .on_click(move |_| click.open_session(&AgentSessionId(asid.clone()))),
        );
    }
    if row.completable {
        let (click, id) = (app.clone(), row.id.clone());
        head = head.child(
            button(tr(app, "Mark done", "标记完成"))
                .variant(ButtonVariant::Secondary)
                .size(ButtonSize::Small)
                .on_click(move |_| click.send(Command::CompleteTask { id: id.clone() })),
        );
    }
    if !row.is_ended() {
        let (click, id) = (app.clone(), row.id.clone());
        head = head.child(
            button(tr(app, "Cancel", "取消"))
                .variant(ButtonVariant::Outline)
                .size(ButtonSize::Small)
                .on_click(move |_| click.send(Command::CancelTask { id: id.clone() })),
        );
    }
    let mut body = div().w_full().flex_col().child(head);
    let detail = row
        .progress
        .as_deref()
        .filter(|_| !row.is_ended())
        .or(row.summary.as_deref())
        .filter(|text| !text.trim().is_empty());
    if let Some(detail) = detail {
        body = body.child(hint(first_line(detail)));
    }
    let meta = meta_line(app, row);
    if !meta.is_empty() {
        body = body.child(hint(meta));
    }
    div()
        .class("mq-set-card")
        .class("mq-agent-section")
        .w_full()
        .flex_col()
        .child(body.class("mq-set-card-body").w_full().flex_col())
}

/// "codex · ~/Work/app · 3 sub-tasks (1 active, 1 failed)".
fn meta_line(app: &App, row: &TaskRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(agent) = row.agent_id.as_deref().filter(|a| !a.is_empty()) {
        parts.push(agent.to_string());
    }
    if let Some(cwd) = row.cwd.as_deref().filter(|c| !c.is_empty()) {
        parts.push(cwd.to_string());
    }
    let kids = row.children;
    if kids.total > 0 {
        let mut text = match app.strings.language() {
            crate::config::Language::En => format!("{} sub-tasks", kids.total),
            crate::config::Language::ZhCn => format!("{} 个子任务", kids.total),
        };
        let mut notes: Vec<String> = Vec::new();
        for (count, en, zh) in [
            (kids.active, "active", "进行中"),
            (kids.waiting, "waiting", "等待中"),
            (kids.failed, "failed", "失败"),
        ] {
            if count > 0 {
                notes.push(format!("{count} {}", tr(app, en, zh)));
            }
        }
        if !notes.is_empty() {
            text.push_str(&format!(" ({})", notes.join(", ")));
        }
        parts.push(text);
    }
    parts.join(" · ")
}

/// The text up to its first line break, so a long goal stays one line.
fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_goal_shows_as_its_first_line() {
        assert_eq!(first_line("fix the test\nthen push"), "fix the test");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn states_are_coloured_by_what_the_owner_must_do() {
        assert_eq!(state_class("done"), "mq-standing--usable");
        assert_eq!(state_class("working"), "mq-standing--checking");
        assert_eq!(state_class("waiting"), "mq-standing--blocked");
        assert_eq!(state_class("failed"), "mq-standing--blocked");
    }
}
