//! The Tasks page: every delegation and command across this computer and
//! the linked ones, as a table with a detail pane. Filter by state and
//! computer, delegate a new goal, open a task's session, confirm a paused
//! one done or cancel one, from the pointer or the keyboard.

use std::collections::HashMap;

use rune::{col, el, row, text, El, TextEdit, Tx};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Keybind, Kit, Menu,
    MenuItem, ScrollBar, SegmentedControl, Spinner, TextInput,
};

use super::copy::{tr, tr_ref};
use super::inbox::keyed;
use super::keymap::Action;
use super::UiState;
use crate::config::Language;
use crate::hub::{TaskRow, TaskStep};
use crate::model::commands::Command;

/// The page's selection, filters, and the histories read so far.
#[derive(Debug, Default)]
pub struct TasksView {
    pub pick: Option<String>,
    pub filter: Filter,
    /// `None`: every computer; `Some(None)`: this one; `Some(Some(name))`:
    /// a linked one.
    pub machine: Option<Option<String>>,
    /// The task Cancel was pressed once for: the next press cancels it.
    pub arm_cancel: Option<String>,
    pub history: HashMap<String, Vec<TaskStep>>,
    /// Why the list could not be read, until it next is.
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Active,
    Paused,
    Done,
    Stopped,
}

impl Filter {
    const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Active,
        Filter::Paused,
        Filter::Done,
        Filter::Stopped,
    ];

    fn word(self, language: Language) -> &'static str {
        match self {
            Filter::All => tr_ref(language, "All", "全部"),
            Filter::Active => tr_ref(language, "Active", "进行中"),
            Filter::Paused => tr_ref(language, "Paused", "已暂停"),
            Filter::Done => tr_ref(language, "Done", "完成"),
            Filter::Stopped => tr_ref(language, "Stopped", "已中止"),
        }
    }

    fn keeps(self, state: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Active => matches!(state, "queued" | "dispatching" | "working"),
            Filter::Paused => matches!(state, "waiting" | "idle_unconfirmed"),
            Filter::Done => state == "done",
            Filter::Stopped => matches!(state, "failed" | "cancelled"),
        }
    }
}

/// A task's state in the App's words (`taskStateWord`).
fn state_word(language: Language, state: &str) -> String {
    let t = |en, zh| tr(language, en, zh);
    match state {
        "queued" => t("Queued", "排队中"),
        "dispatching" => t("Starting", "启动中"),
        "working" => t("Working", "工作中"),
        "waiting" => t("Needs you", "等你处理"),
        "idle_unconfirmed" => t("Paused", "已暂停"),
        "done" => t("Done", "完成"),
        "failed" => t("Failed", "失败"),
        "cancelled" => t("Cancelled", "已取消"),
        other => other.replace('_', " "),
    }
}

/// The state's quiet mark: a spinner while it works, else a small dot.
fn state_mark(k: &Kit, state: &str) -> El {
    let color = match state {
        "working" | "dispatching" | "queued" => k.c.text_accent,
        "waiting" | "idle_unconfirmed" => k.c.warning.icon,
        "done" => k.c.success.icon,
        "failed" => k.c.danger.icon,
        _ => k.c.text_faint,
    };
    let mark = if state == "working" {
        Spinner::new().size(10.0).color(color).build(k)
    } else {
        el().size(7.0, 7.0).radius(3.5).fill(color)
    };
    row().size(12.0, 12.0).center().child(mark).a11y_hidden()
}

fn state_cell(k: &Kit, language: Language, state: &str) -> El {
    row().gap(6.0).center_y().child(state_mark(k, state)).child(
        text(state_word(language, state))
            .font(k.ty.small)
            .color(k.c.text_muted)
            .truncate()
            .shrink()
            .min_w(0.0),
    )
}

/// Where the task runs: its agent, or the computer a mirror runs on.
fn place(s: &UiState, t: &TaskRow) -> String {
    let language = s.cfg.general.language;
    match (&t.machine, &t.agent_id) {
        (Some(m), _) => format!("→ {m}"),
        (None, Some(a)) => s.agent_name(a),
        (None, None) if t.kind == "command" => tr(language, "Terminal", "终端"),
        _ => String::new(),
    }
}

/// How long it has run: to its last update once it ended, to now before.
fn duration(t: &TaskRow, now: u64, language: Language) -> String {
    if t.started_ms == 0 {
        return String::new();
    }
    let end = if t.is_ended() { t.updated_ms } else { now };
    super::running::elapsed(end.saturating_sub(t.started_ms), language)
}

/// The tasks the filters keep, in the gateway's order (newest update first).
fn shown(s: &UiState) -> Vec<&TaskRow> {
    let v = &s.tasks_view;
    s.tasks
        .iter()
        .filter(|t| v.filter.keeps(&t.state))
        .filter(|t| v.machine.as_ref().is_none_or(|m| *m == t.machine))
        .collect()
}

fn current(list: &[&TaskRow], view: &TasksView) -> Option<usize> {
    if list.is_empty() {
        return None;
    }
    Some(
        view.pick
            .as_ref()
            .and_then(|p| list.iter().position(|t| &t.id == p))
            .unwrap_or(0),
    )
}

/// The selected task, as the page shows it.
fn selected(s: &UiState) -> Option<&TaskRow> {
    let list = shown(s);
    current(&list, &s.tasks_view).map(|i| list[i])
}

fn select(s: &mut UiState, id: String) {
    if s.tasks_view.arm_cancel.as_ref() != Some(&id) {
        s.tasks_view.arm_cancel = None;
    }
    s.send(Command::TaskHistory { id: id.clone() });
    s.tasks_view.pick = Some(id);
}

/// A new list arrived: it reads again, and the selected task's history
/// follows it while the page is open.
pub fn listed(s: &mut UiState) {
    s.tasks_view.error = None;
    if s.mode != super::Mode::Tasks {
        return;
    }
    if let Some(id) = selected(s).map(|t| t.id.clone()) {
        s.send(Command::TaskHistory { id });
    }
}

fn cancel(s: &mut UiState, id: &str) {
    if s.tasks_view.arm_cancel.as_deref() == Some(id) {
        s.tasks_view.arm_cancel = None;
        s.send(Command::CancelTask { id: id.to_string() });
    } else {
        s.tasks_view.arm_cancel = Some(id.to_string());
    }
}

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

/// The Tasks keys (`keymap::run`): j/k and the arrows move, Enter opens
/// the session, M confirms a paused task done, X twice cancels.
pub fn key(s: &mut UiState, action: Action) -> bool {
    let list = shown(s);
    let Some(at) = current(&list, &s.tasks_view) else {
        return false;
    };
    let t = list[at];
    match action {
        Action::NextRow | Action::PrevRow => {
            let to = if action == Action::NextRow {
                (at + 1).min(list.len() - 1)
            } else {
                at.saturating_sub(1)
            };
            let id = list[to].id.clone();
            select(s, id);
        }
        Action::OpenRow => {
            let Some(asid) = t.asid.clone() else {
                return false;
            };
            s.open_session(&asid);
        }
        Action::MarkDone if t.completable => {
            let id = t.id.clone();
            s.send(Command::CompleteTask { id });
        }
        Action::CancelTask if !t.is_ended() => {
            let id = t.id.clone();
            cancel(s, &id);
        }
        _ => return false,
    }
    true
}

// ---- the view ------------------------------------------------------------------------

/// Columns that fold away as the table narrows.
#[derive(Clone, Copy)]
struct Cols {
    agent: bool,
    updated: bool,
    duration: bool,
}

impl Cols {
    fn for_width(w: f32) -> Cols {
        Cols {
            agent: w >= 520.0,
            updated: w >= 400.0,
            duration: w >= 600.0,
        }
    }
}

const AGENT_W: f32 = 128.0;
const STATE_W: f32 = 104.0;
const TIME_W: f32 = 64.0;

pub fn view(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    // On a very wide card the table and its detail stay together.
    let card_w = rune::memo::layout_with("main", |r| r[2])
        .unwrap_or(1080.0)
        .min(MAX_W);
    let list = shown(s);
    let at = current(&list, &s.tasks_view);
    let detail_w = (card_w * 0.36).clamp(300.0, 440.0);
    let table_w = card_w - detail_w - 1.0;
    let mut body = row().key("tasks-body").w_full().grow(1.0).min_h(0.0);
    if s.tasks.is_empty() {
        body = body.child(empty(s, k));
    } else {
        body = body
            .child(table(s, k, &list, at, Cols::for_width(table_w - 56.0)))
            .child(
                el().key("tasks-rule")
                    .w(1.0)
                    .h_full()
                    .fill(k.c.border_weak)
                    .a11y_hidden(),
            )
            .child(match at {
                Some(i) => detail(s, k, list[i], detail_w),
                None => col()
                    .key("task-detail")
                    .w(detail_w)
                    .h_full()
                    .center()
                    .child(
                        text(tr_ref(language, "No task selected", "未选择任务"))
                            .font(k.ty.small)
                            .color(k.c.text_faint),
                    ),
            });
    }
    let page = col()
        .key("tasks")
        .max_w(MAX_W)
        .child(head(s, k, card_w < STACK_AT))
        .children((!s.tasks.is_empty()).then(|| filters(s, k, layer)))
        .children(error_line(s, k))
        .child(body);
    super::card_stage(
        "tasks",
        row()
            .key("tasks-centre")
            .child(el().grow(1.0))
            .child(page.w_full())
            .child(el().grow(1.0)),
    )
}

/// The page's widest: past it the table would stretch its title column.
const MAX_W: f32 = 1760.0;
/// Under this card width the delegate field goes under the title.
const STACK_AT: f32 = 900.0;

fn head(s: &UiState, k: &Kit, stacked: bool) -> El {
    let language = s.cfg.general.language;
    let placeholder = match &s.task_cwd {
        Some(cwd) => tr(
            language,
            &format!("Delegate a goal… (runs in {cwd})"),
            &format!("委派一个目标…（在 {cwd} 运行）"),
        ),
        None => tr(language, "Delegate a goal…", "委派一个目标…"),
    };
    let delegate = if stacked {
        row().w_full()
    } else {
        row().w(460.0)
    };
    let delegate = delegate
        .key("task-new")
        .shrink()
        .min_w(240.0)
        .gap(8.0)
        .center_y()
        .child(
            TextInput::new(&s.task_goal)
                .placeholder(placeholder)
                .label(tr_ref(language, "Task goal", "任务目标"))
                .on_edit(|s: &mut UiState| &mut s.task_goal)
                .on_submit(|s: &mut UiState, _: &mut Tx| create(s))
                .build(k)
                .key("task-goal")
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        )
        // The button shows once there is a goal to send; Enter sends it too.
        .children((!s.task_goal.text.trim().is_empty()).then(|| {
            Button::new(tr_ref(language, "Delegate", "委派"))
                .variant(ButtonVariant::Submit)
                .shortcut("↵")
                .on_press(create)
                .build(k)
                .key("task-create")
        }));
    let line = if stacked { col() } else { row().center_y() };
    line.key("tasks-head")
        .w_full()
        .px(28.0)
        .pt(26.0)
        .pb(14.0)
        .gap(if stacked { 12.0 } else { 24.0 })
        .child(
            col()
                .key("tasks-title")
                .grow(if stacked { 0.0 } else { 1.0 })
                .shrink()
                .min_w(0.0)
                .gap(4.0)
                .child(
                    text(tr_ref(language, "Tasks", "任务"))
                        .key("page-title")
                        .font(rune::TextStyle {
                            size: 22.0,
                            line_height: 28.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base),
                )
                .child(
                    text(tr_ref(
                        language,
                        "Work handed to agents, here and on linked computers.",
                        "交给 Agent 的工作，包括本机与已连接的电脑。",
                    ))
                    .key("page-subtitle")
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .truncate(),
                ),
        )
        .child(delegate)
}

fn filters(s: &UiState, k: &Kit, layer: &mut Vec<El>) -> El {
    let language = s.cfg.general.language;
    let v = &s.tasks_view;
    let labels: Vec<String> = Filter::ALL
        .iter()
        .map(|f| {
            let n = s.tasks.iter().filter(|t| f.keeps(&t.state)).count();
            if *f == Filter::All || n == 0 {
                f.word(language).to_string()
            } else {
                format!("{} {n}", f.word(language))
            }
        })
        .collect();
    let at = Filter::ALL.iter().position(|f| *f == v.filter);
    let mut bar = row()
        .key("tasks-filters")
        .w_full()
        .px(28.0)
        .pb(12.0)
        .gap(12.0)
        .center_y()
        .flex_wrap()
        .child(
            SegmentedControl::new(&labels, at)
                .fit()
                .label(tr_ref(language, "State", "状态"))
                .on_pick(|s: &mut UiState, i, _: &mut Tx| {
                    s.tasks_view.filter = Filter::ALL[i];
                    s.tasks_view.arm_cancel = None;
                })
                .build(k)
                .key("tasks-state"),
        );
    let mut machines: Vec<String> = s.tasks.iter().filter_map(|t| t.machine.clone()).collect();
    for comp in s.all_computers() {
        if comp.server_id != "local" && comp.server_id != "self" && comp.server_id != "all" {
            machines.push(comp.name.clone());
        }
    }
    machines.sort();
    machines.dedup();
    if !machines.is_empty() {
        let open = s.menu == Some(super::OpenMenu::TasksComputerPicker);
        let (icon_glyph, display_name) = match &v.machine {
            None => (
                &super::parts::glyphs::NETWORK,
                tr(language, "All computers", "所有电脑"),
            ),
            Some(None) => (
                &super::parts::glyphs::COMPUTER,
                tr(language, "This computer", "本机"),
            ),
            Some(Some(m)) => (&super::parts::glyphs::COMPUTER, m.clone()),
        };

        let trigger = rune::row()
            .key("tasks-computer-picker-trigger")
            .h(32.0)
            .px(10.0)
            .gap(8.0)
            .center_y()
            .radius(k.radius.md)
            .fill(k.c.layer_01)
            .border(
                1.0,
                if open {
                    k.c.border_focus
                } else {
                    k.c.border_weak
                },
            )
            .cursor(rune::Cursor::Pointer)
            .role("button")
            .label(tr_ref(language, "Filter by computer", "按电脑筛选"))
            .child(rune::icon(icon_glyph, 14.0).color(k.c.text_base))
            .child(
                rune::text(display_name)
                    .font(k.ty.ui_medium)
                    .color(k.c.text_base),
            )
            .child(
                rune::text(if open { "▴" } else { "▾" })
                    .font(k.ty.small)
                    .color(k.c.text_muted),
            )
            .on_press(|s: &mut UiState| {
                let open = s.menu == Some(super::OpenMenu::TasksComputerPicker);
                s.menu = (!open).then_some(super::OpenMenu::TasksComputerPicker);
            });

        let mut menu = Menu::new("tasks-computer-picker-menu")
            .trigger(trigger)
            .open(open)
            .min_width(200.0)
            .placement(rune_kit::Placement::BottomStart)
            .on_close(|s: &mut UiState| s.menu = None);

        menu = menu.item(
            MenuItem::new(super::copy::tr_ref(language, "All computers", "所有电脑"))
                .lead(rune::icon(&super::parts::glyphs::NETWORK, 15.0).color(k.c.text_accent))
                .checked(v.machine.is_none())
                .on_select(|s: &mut UiState| {
                    s.menu = None;
                    s.tasks_view.machine = None;
                }),
        );

        menu = menu.item(
            MenuItem::new(super::copy::tr_ref(language, "This computer", "本机"))
                .lead(rune::icon(&super::parts::glyphs::COMPUTER, 15.0).color(k.c.text_base))
                .checked(v.machine == Some(None))
                .on_select(|s: &mut UiState| {
                    s.menu = None;
                    s.tasks_view.machine = Some(None);
                }),
        );

        menu = menu.separator();
        for m in machines {
            let is_curr = v.machine == Some(Some(m.clone()));
            let target = m.clone();
            menu = menu.item(
                MenuItem::new(m)
                    .lead(rune::icon(&super::parts::glyphs::COMPUTER, 15.0).color(k.c.text_base))
                    .checked(is_curr)
                    .on_select(move |s: &mut UiState| {
                        s.menu = None;
                        s.tasks_view.machine = Some(Some(target.clone()));
                    }),
            );
        }

        bar = bar.child(menu.build(k, layer).key("tasks-machine"));
    }
    bar.child(el().grow(1.0)).child(
        IconButton::new(&super::icons::REFRESH)
            .variant(IconButtonVariant::Ghost)
            .label(tr_ref(language, "Refresh", "刷新"))
            .help(tr_ref(language, "Refresh", "刷新"))
            .on_press(|s: &mut UiState| s.send(Command::RefreshTasks))
            .build(k)
            .key("task-refresh"),
    )
}

/// The list could not be read: why, and a retry, over the last list.
fn error_line(s: &UiState, k: &Kit) -> Option<El> {
    let why = s.tasks_view.error.as_ref()?;
    let language = s.cfg.general.language;
    Some(
        row()
            .key("tasks-error")
            .w_full()
            .px(28.0)
            .pb(10.0)
            .gap(8.0)
            .center_y()
            .child(
                el().size(7.0, 7.0)
                    .radius(3.5)
                    .fill(k.c.warning.icon)
                    .a11y_hidden(),
            )
            .child(
                text(tr(
                    language,
                    &format!("Couldn't refresh tasks: {why}"),
                    &format!("无法刷新任务：{why}"),
                ))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate()
                .help_when_truncated(why.clone())
                .shrink()
                .min_w(0.0),
            )
            .child(
                Button::new(tr_ref(language, "Retry", "重试"))
                    .variant(ButtonVariant::Ghost)
                    .control_size(ControlSize::Small)
                    .on_press(|s: &mut UiState| s.send(Command::RefreshTasks))
                    .build(k)
                    .key("tasks-retry"),
            ),
    )
}

fn header_cell(k: &Kit, label: &str, w: Option<f32>, end: bool) -> El {
    let t = text(label.to_string())
        .font(k.ty.small_medium)
        .color(k.c.text_faint)
        .truncate();
    let cell = row().center_y();
    let cell = match w {
        Some(w) => cell.w(w),
        None => cell.grow(1.0).shrink().min_w(0.0),
    };
    if end {
        cell.child(el().grow(1.0)).child(t)
    } else {
        cell.child(t)
    }
}

fn table(s: &UiState, k: &Kit, list: &[&TaskRow], at: Option<usize>, cols: Cols) -> El {
    let language = s.cfg.general.language;
    let now = super::now_ms();
    let mut header = row()
        .key("tasks-columns")
        .w_full()
        .h(28.0)
        .px(12.0)
        .gap(12.0)
        .center_y()
        .child(header_cell(
            k,
            tr_ref(language, "Task", "任务"),
            None,
            false,
        ));
    if cols.agent {
        header = header.child(header_cell(
            k,
            tr_ref(language, "Runs on", "运行于"),
            Some(AGENT_W),
            false,
        ));
    }
    header = header.child(header_cell(
        k,
        tr_ref(language, "State", "状态"),
        Some(STATE_W),
        false,
    ));
    if cols.updated {
        header = header.child(header_cell(
            k,
            tr_ref(language, "Updated", "更新"),
            Some(TIME_W),
            true,
        ));
    }
    if cols.duration {
        header = header.child(header_cell(
            k,
            tr_ref(language, "Duration", "耗时"),
            Some(TIME_W),
            true,
        ));
    }
    let mut rows = col()
        .key("tasks-rows")
        .w_full()
        .pt(4.0)
        .pb(16.0)
        .gap(1.0)
        .role("list")
        .label(tr_ref(language, "Tasks", "任务"));
    if list.is_empty() {
        let word = s.tasks_view.filter.word(language);
        rows = rows.child(
            text(tr(
                language,
                &format!("Nothing in “{word}”."),
                &format!("“{word}”中没有任务。"),
            ))
            .key("tasks-none")
            .font(k.ty.small)
            .color(k.c.text_faint)
            .px(12.0)
            .py(16.0),
        );
    }
    for (i, t) in list.iter().enumerate() {
        rows = rows.child(task_row(s, k, t, at == Some(i), cols, now));
    }
    col()
        .key("tasks-table")
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h_full()
        .min_h(0.0)
        .px(16.0)
        .child(header)
        .child(
            el().key("tasks-columns-rule")
                .w_full()
                .h(1.0)
                .fill(k.c.border_weak)
                .a11y_hidden(),
        )
        .child(
            col()
                .key("tasks-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(rows)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        )
        .child(foot(s, k, cols.agent))
}

fn time_cell(k: &Kit, value: String) -> El {
    row()
        .w(TIME_W)
        .center_y()
        .child(el().grow(1.0))
        .child(text(value).font(k.ty.small).color(k.c.text_faint))
}

fn task_row(s: &UiState, k: &Kit, t: &TaskRow, selected: bool, cols: Cols, now: u64) -> El {
    let language = s.cfg.general.language;
    let id = t.id.clone();
    let child = t
        .parent_task_id
        .as_ref()
        .is_some_and(|p| s.tasks.iter().any(|x| &x.id == p));
    let mut title = row().grow(1.0).shrink().min_w(0.0).gap(6.0).center_y();
    if child {
        title = title.child(text("↳").font(k.ty.small).color(k.c.text_faint));
    }
    title = title.child(
        text(t.title.clone())
            .font(k.ty.ui)
            .color(if selected {
                k.c.text_base
            } else if t.is_ended() {
                k.c.text_muted
            } else {
                k.c.text_base
            })
            .truncate()
            .help_when_truncated(t.title.clone())
            .shrink()
            .min_w(0.0),
    );
    let mut line = row()
        .key(format!("task-{}", t.id))
        .w_full()
        .h(34.0)
        .px(12.0)
        .gap(12.0)
        .center_y()
        .radius(k.radius.md)
        .role("listitem")
        .label(t.title.clone())
        .focus_selected(selected)
        .child(title);
    if cols.agent {
        let mut cell = row().w(AGENT_W).gap(6.0).center_y();
        if let (None, Some(agent)) = (&t.machine, &t.agent_id) {
            cell = cell.child(super::agents::mark(agent, 12.0, s.dark).a11y_hidden());
        }
        cell = cell.child(
            text(place(s, t))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .truncate()
                .shrink()
                .min_w(0.0),
        );
        line = line.child(cell);
    }
    line = line.child(state_cell(k, language, &t.state).w(STATE_W));
    if cols.updated {
        line = line.child(time_cell(k, super::ago(t.updated_ms, now, language)));
    }
    if cols.duration {
        line = line.child(time_cell(k, duration(t, now, language)));
    }
    line = k.interactive(line);
    if selected {
        line = line.fill(k.c.pressed);
    }
    line.on_press(move |s: &mut UiState| select(s, id.clone()))
}

/// The table's foot: the keys that work here.
fn foot(s: &UiState, k: &Kit, wide: bool) -> El {
    let language = s.cfg.general.language;
    let hint = |keys: &[&str], what: &str| {
        row()
            .gap(6.0)
            .center_y()
            .child(Keybind::new(keys).build(k))
            .child(
                text(what.to_string())
                    .font(k.ty.small)
                    .color(k.c.text_faint),
            )
    };
    let line = row()
        .key("tasks-hints")
        .w_full()
        .h(FOOT_H)
        .px(12.0)
        .gap(16.0)
        .center_y()
        .child(hint(&["J", "K"], tr_ref(language, "Move", "移动")))
        .child(hint(&["↵"], tr_ref(language, "Open session", "打开会话")))
        .children(wide.then(|| hint(&["M"], tr_ref(language, "Mark done", "标记完成"))))
        .children(wide.then(|| hint(&["X"], tr_ref(language, "Cancel", "取消"))));
    col()
        .key("tasks-foot")
        .w_full()
        .child(
            el().key("tasks-foot-rule")
                .w_full()
                .h(1.0)
                .fill(k.c.border_weak)
                .a11y_hidden(),
        )
        .child(line)
}

/// Both panes' feet: the hints under the table, the actions under the detail.
const FOOT_H: f32 = 52.0;

fn section(k: &Kit, key: &str, title: &str, body: El) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .gap(6.0)
        .child(
            text(title.to_string())
                .font(k.ty.small_medium)
                .color(k.c.text_faint),
        )
        .child(body)
}

fn fact(k: &Kit, label: &str, value: String) -> El {
    row()
        .w_full()
        .gap(12.0)
        .child(
            text(label.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint)
                .w(76.0),
        )
        .child(
            text(value)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .grow(1.0)
                .shrink()
                .min_w(0.0),
        )
}

fn detail(s: &UiState, k: &Kit, t: &TaskRow, w: f32) -> El {
    let language = s.cfg.general.language;
    let now = super::now_ms();
    let kind = if t.kind == "command" {
        tr(language, "Command", "命令")
    } else {
        tr(language, "Delegation", "委派")
    };
    let mut body = col()
        .key(format!("task-detail-{}", t.id))
        .w_full()
        .px(24.0)
        .pt(26.0)
        .pb(20.0)
        .gap(18.0)
        .child(
            col()
                .key("task-head")
                .w_full()
                .gap(6.0)
                .child(
                    row()
                        .key("task-state")
                        .gap(6.0)
                        .center_y()
                        .child(state_mark(k, &t.state))
                        .child(
                            text(format!("{} · {kind}", state_word(language, &t.state)))
                                .font(k.ty.small_medium)
                                .color(k.c.text_muted),
                        ),
                )
                .child(
                    text(t.title.clone())
                        .key("task-title")
                        .font(rune::TextStyle {
                            size: 18.0,
                            line_height: 24.0,
                            ..k.ty.ui_medium
                        })
                        .color(k.c.text_base)
                        .wrap()
                        .selectable(true)
                        .w_full(),
                ),
        );
    let mut facts = col().key("task-facts").w_full().gap(4.0);
    let where_ = place(s, t);
    if !where_.is_empty() {
        facts = facts.child(fact(k, tr_ref(language, "Runs on", "运行于"), where_));
    }
    if t.started_ms > 0 {
        facts = facts.child(fact(
            k,
            tr_ref(language, "Started", "开始"),
            super::ago(t.started_ms, now, language),
        ));
    }
    if t.updated_ms > 0 {
        facts = facts.child(fact(
            k,
            tr_ref(language, "Updated", "更新"),
            super::ago(t.updated_ms, now, language),
        ));
    }
    let ran = duration(t, now, language);
    if !ran.is_empty() {
        facts = facts.child(fact(k, tr_ref(language, "Duration", "耗时"), ran));
    }
    if let Some(code) = t.exit_code {
        facts = facts.child(fact(
            k,
            tr_ref(language, "Exit code", "退出码"),
            code.to_string(),
        ));
    }
    body = body.child(facts);
    if !t.goal.trim().is_empty() && t.goal.trim() != t.title.trim() {
        body = body.child(section(
            k,
            "task-goal-text",
            if t.kind == "command" {
                tr_ref(language, "Command line", "命令行")
            } else {
                tr_ref(language, "Goal", "目标")
            },
            text(t.goal.clone())
                .font(if t.kind == "command" {
                    k.ty.mono
                } else {
                    k.ty.ui
                })
                .color(k.c.text_base)
                .wrap_anywhere()
                .selectable(true)
                .w_full(),
        ));
    }
    if let Some(now_doing) = t.progress.as_ref().filter(|p| !p.is_empty()) {
        body = body.child(section(
            k,
            "task-progress",
            tr_ref(language, "Now", "当前"),
            text(now_doing.clone())
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .w_full(),
        ));
    }
    if let Some(summary) = t.summary.as_ref().filter(|p| !p.is_empty()) {
        body = body.child(section(
            k,
            "task-summary",
            tr_ref(language, "Summary", "总结"),
            text(summary.clone())
                .font(k.ty.ui)
                .color(k.c.text_base)
                .wrap()
                .selectable(true)
                .w_full(),
        ));
    }
    if t.children.total > 0 {
        body = body.child(children(s, k, t));
    }
    if let Some(steps) = s.tasks_view.history.get(&t.id).filter(|h| !h.is_empty()) {
        body = body.child(history(s, k, steps, now));
    }
    col()
        .key("task-detail")
        .w(w)
        .h_full()
        .min_h(0.0)
        .child(
            col()
                .key("task-detail-scroll")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(s.look().arrive(body, 0))
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_bar(k),
        )
        .child(actions(s, k, t))
}

fn children(s: &UiState, k: &Kit, t: &TaskRow) -> El {
    let language = s.cfg.general.language;
    let c = &t.children;
    let mut parts = vec![tr(
        language,
        &format!(
            "{} sub-task{}",
            c.total,
            if c.total == 1 { "" } else { "s" }
        ),
        &format!("{} 个子任务", c.total),
    )];
    if c.active > 0 {
        parts.push(tr(
            language,
            &format!("{} active", c.active),
            &format!("{} 个进行中", c.active),
        ));
    }
    if c.waiting > 0 {
        parts.push(tr(
            language,
            &format!("{} waiting", c.waiting),
            &format!("{} 个等待中", c.waiting),
        ));
    }
    if c.failed > 0 {
        parts.push(tr(
            language,
            &format!("{} failed", c.failed),
            &format!("{} 个失败", c.failed),
        ));
    }
    let mut list = col().key("task-children").w_full().gap(2.0).child(
        text(parts.join(" · "))
            .font(k.ty.small)
            .color(k.c.text_muted),
    );
    for child in s
        .tasks
        .iter()
        .filter(|x| x.parent_task_id.as_deref() == Some(t.id.as_str()))
    {
        let id = child.id.clone();
        list = list.child(
            k.interactive(
                row()
                    .key(format!("child-{}", child.id))
                    .w_full()
                    .h(28.0)
                    .px(6.0)
                    .gap(8.0)
                    .center_y()
                    .radius(k.radius.md)
                    .child(state_mark(k, &child.state))
                    .child(
                        text(child.title.clone())
                            .font(k.ty.small)
                            .color(k.c.text_base)
                            .truncate()
                            .help_when_truncated(child.title.clone())
                            .shrink()
                            .min_w(0.0),
                    ),
            )
            .on_press(move |s: &mut UiState| select(s, id.clone())),
        );
    }
    section(
        k,
        "task-subtasks",
        tr_ref(language, "Sub-tasks", "子任务"),
        list,
    )
}

/// The latest state changes, newest first, as a thin timeline.
fn history(s: &UiState, k: &Kit, steps: &[TaskStep], now: u64) -> El {
    let language = s.cfg.general.language;
    let mut list = col().key("task-history").w_full().gap(8.0);
    for (i, step) in steps.iter().rev().take(8).enumerate() {
        let mut words = col().grow(1.0).shrink().min_w(0.0).gap(2.0).child(
            text(state_word(language, &step.to))
                .font(k.ty.small_medium)
                .color(k.c.text_base),
        );
        if let Some(note) = step.note.as_ref().filter(|n| !n.trim().is_empty()) {
            words = words.child(
                text(note.clone())
                    .font(k.ty.small)
                    .color(k.c.text_muted)
                    .wrap()
                    .w_full(),
            );
        }
        list = list.child(
            row()
                .key(format!("step-{i}"))
                .w_full()
                .gap(10.0)
                .child(state_mark(k, &step.to).mt(2.0))
                .child(words)
                .child(
                    text(super::ago(step.at_ms, now, language))
                        .font(k.ty.small)
                        .color(k.c.text_faint),
                ),
        );
    }
    section(
        k,
        "task-timeline",
        tr_ref(language, "Timeline", "时间线"),
        list,
    )
}

fn actions(s: &UiState, k: &Kit, t: &TaskRow) -> El {
    let language = s.cfg.general.language;
    let mut line = row()
        .key("task-actions")
        .w_full()
        .min_h(FOOT_H)
        .py(8.0)
        .px(20.0)
        .gap(8.0)
        .center_y()
        .flex_wrap();
    if let Some(asid) = t.asid.clone() {
        let label = tr(language, "Open session", "打开会话");
        line = line.child(
            Button::new(label.clone())
                .variant(ButtonVariant::Outline)
                .label_with(keyed(label, &["↵"]))
                .on_press(move |s: &mut UiState| s.open_session(&asid))
                .build(k)
                .key("task-open"),
        );
    }
    if t.completable {
        let id = t.id.clone();
        let label = tr(language, "Mark done", "标记完成");
        line = line.child(
            Button::new(label.clone())
                .variant(ButtonVariant::Submit)
                .label_with(keyed(label, &["M"]))
                .on_press(move |s: &mut UiState| s.send(Command::CompleteTask { id: id.clone() }))
                .build(k)
                .key("task-done"),
        );
    }
    if !t.is_ended() {
        let armed = s.tasks_view.arm_cancel.as_deref() == Some(t.id.as_str());
        let id = t.id.clone();
        let label = if armed {
            tr(language, "Confirm cancel", "确认取消")
        } else {
            tr(language, "Cancel task", "取消任务")
        };
        line = line.child(
            Button::new(label.clone())
                .variant(if armed {
                    ButtonVariant::Danger
                } else {
                    ButtonVariant::Ghost
                })
                .label_with(keyed(label, &["X"]))
                .on_press(move |s: &mut UiState| cancel(s, &id))
                .build(k)
                .key("task-cancel"),
        );
    }
    col()
        .key("task-foot")
        .w_full()
        .child(
            el().key("task-foot-rule")
                .w_full()
                .h(1.0)
                .fill(k.c.border_weak)
                .a11y_hidden(),
        )
        .child(line)
}

/// No tasks at all: a calm line, or why the list could not be read.
fn empty(s: &UiState, k: &Kit) -> El {
    let language = s.cfg.general.language;
    let (title, detail) = if s.tasks_view.error.is_some() {
        (
            tr_ref(language, "Tasks are unavailable", "暂时无法读取任务"),
            tr_ref(
                language,
                "The gateway didn't answer. Retry, or check that it is running.",
                "Gateway 没有响应。请重试，或检查它是否在运行。",
            ),
        )
    } else {
        (
            tr_ref(language, "No tasks yet", "还没有任务"),
            tr_ref(
                language,
                "Delegate a goal above, or ask an agent to hand work to another computer.",
                "在上方委派一个目标，或让 Agent 把工作交给另一台电脑。",
            ),
        )
    };
    col()
        .key("tasks-empty")
        .w_full()
        .h_full()
        .center()
        .gap(8.0)
        .child(text(title).font(k.ty.ui_medium).color(k.c.text_base))
        .child(
            text(detail)
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap()
                .max_w(420.0),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::commands;

    #[test]
    fn cancel_needs_a_second_press_and_moving_disarms_it() {
        let (tx, mut rx) = commands::channel();
        let mut s = UiState::new(tx);
        s.mode = super::super::Mode::Tasks;
        for id in ["a", "b"] {
            s.tasks.push(TaskRow {
                id: id.into(),
                state: "working".into(),
                ..TaskRow::default()
            });
        }
        let cancels = |rx: &mut commands::CommandReceiver| {
            std::iter::from_fn(|| rx.try_recv().ok())
                .filter(|c| matches!(c, Command::CancelTask { .. }))
                .count()
        };
        assert!(key(&mut s, Action::CancelTask));
        assert_eq!(cancels(&mut rx), 0);
        // Moving away disarms it: the next X on b only arms b.
        assert!(key(&mut s, Action::NextRow));
        assert!(key(&mut s, Action::CancelTask));
        assert_eq!(cancels(&mut rx), 0);
        assert!(key(&mut s, Action::CancelTask));
        assert_eq!(cancels(&mut rx), 1);
    }
}
