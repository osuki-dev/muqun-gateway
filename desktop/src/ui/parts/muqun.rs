//! Muqun's own MCP tools (exec.run, tasks.*, fleet.*, files.present), drawn
//! from the App's presentation (`muqun-tool-presentation.ts`,
//! `agent-muqun-tool-body.tsx`). A presented file is a row that opens it.

use muqun_gateway::agents::{TimelineItem, ToolCall};
use rune::{col, icon, row, text, Cursor, El};

use super::{caption, chip, fact, glyphs, mono, CardState, Tcx, ToolCard};
use crate::model::tool_view::{self as tv, Muqun, MuqunOp, MuqunStatus};
use crate::ui::UiState;

fn op_word(cx: &Tcx, op: MuqunOp) -> &'static str {
    match op {
        MuqunOp::ExecRun => cx.t("Run command", "运行命令"),
        MuqunOp::FilesPresent => cx.t("Preview file", "预览文件"),
        MuqunOp::TasksDelegate => cx.t("Start task", "开始任务"),
        MuqunOp::TasksWait => cx.t("Wait for task", "等待任务"),
        MuqunOp::TasksRead => cx.t("Read task", "读取任务"),
        MuqunOp::TasksCancel => cx.t("Cancel task", "取消任务"),
        MuqunOp::TasksComplete => cx.t("Complete task", "完成任务"),
        MuqunOp::FleetList => cx.t("Computers", "电脑"),
        MuqunOp::FleetDescribe => cx.t("Computer details", "电脑详情"),
    }
}

fn status_word(cx: &Tcx, status: MuqunStatus) -> &'static str {
    match status {
        MuqunStatus::Calling => cx.t("Running", "运行中"),
        MuqunStatus::Started => cx.t("Started", "已启动"),
        MuqunStatus::Working => cx.t("Running when checked", "查询时运行中"),
        MuqunStatus::Queued => cx.t("Queued when checked", "查询时排队中"),
        MuqunStatus::Waiting => cx.t("Waiting when checked", "查询时等待中"),
        MuqunStatus::Unconfirmed => cx.t("Completion unconfirmed", "尚未确认完成"),
        MuqunStatus::Done => cx.t("Completed", "已完成"),
        MuqunStatus::Failed => cx.t("Failed", "失败"),
        MuqunStatus::Cancelled => cx.t("Cancelled", "已取消"),
        MuqunStatus::Returned => cx.t("Received", "已收到"),
        MuqunStatus::Unassigned => cx.t("No assigned task", "没有分配的任务"),
        MuqunStatus::Unknown => cx.t("Unknown", "未知"),
    }
}

fn glyph(op: MuqunOp) -> &'static rune::Icon {
    match op {
        MuqunOp::ExecRun => &glyphs::TERMINAL,
        MuqunOp::FilesPresent => &glyphs::DOCUMENT,
        MuqunOp::FleetList | MuqunOp::FleetDescribe => &glyphs::COMPUTER,
        _ => &glyphs::TASK_LIST,
    }
}

fn size_label(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

/// A presented file: its name and folder, a click opens it.
fn file_row(cx: &Tcx, i: usize, file: &tv::PresentedFile) -> El {
    let k = cx.k;
    let path = file.path.clone();
    let mut meta = tv::dirname(&file.path).to_string();
    if let Some(size) = file.size {
        meta = format!("{} · {meta}", size_label(size));
    }
    k.interactive(
        row()
            .key(format!("file-{i}"))
            .w_full()
            .gap(10.0)
            .px(10.0)
            .py(8.0)
            .center_y()
            .radius(k.radius.md)
            .border(1.0, k.c.border_weak)
            .fill(k.c.surface_base)
            .child(
                row()
                    .size(30.0, 30.0)
                    .center()
                    .radius(k.radius.sm)
                    .fill(k.alpha(k.c.icon_accent, 0.12))
                    .child(icon(&glyphs::DOCUMENT, 15.0).color(k.c.icon_accent)),
            )
            .child(
                col()
                    .gap(1.0)
                    .shrink()
                    .grow(1.0)
                    .child(
                        text(file.name.clone())
                            .font(k.ty.ui_medium)
                            .color(k.c.text_base)
                            .single_line()
                            .truncate_middle(),
                    )
                    .child(
                        text(meta)
                            .font(k.ty.small)
                            .color(k.c.text_faint)
                            .single_line()
                            .truncate_head(),
                    ),
            )
            .child(icon(&glyphs::OPEN, 13.0).color(k.c.icon_muted)),
    )
    .cursor(Cursor::Pointer)
    .label(file.name.clone())
    .help(file.path.clone())
    .on_press(move |s: &mut UiState| s.open_timeline_file(&path))
}

pub fn card(cx: &Tcx, item: &TimelineItem, call: &ToolCall, m: &Muqun) -> El {
    let k = cx.k;
    let mut card = ToolCard::new(&item.id, glyph(m.op), op_word(cx, m.op));
    card.accent = Some(k.c.icon_accent);
    card.title = match &m.machine {
        Some(machine) if machine != "self" => machine.clone(),
        Some(_) => cx.t("This computer", "本机").to_string(),
        None => String::new(),
    };
    card.caption = m
        .command
        .clone()
        .or_else(|| m.goal.clone().map(|g| tv::first_line(&g).to_string()))
        .or_else(|| m.files.first().map(|f| f.name.clone()))
        .unwrap_or_default();
    let tone = match m.status {
        MuqunStatus::Failed => k.c.danger.fg,
        MuqunStatus::Done => k.c.success.fg,
        _ => k.c.text_muted,
    };
    if m.status != MuqunStatus::Calling {
        card.chips.push(chip(k, status_word(cx, m.status), tone));
    }
    card.state = match m.status {
        MuqunStatus::Calling if cx.stopped() => CardState::Stopped,
        MuqunStatus::Calling => CardState::Running,
        MuqunStatus::Failed => CardState::Failed,
        _ => CardState::Done,
    };
    // A presented file is the point of the call: always in view.
    if !m.files.is_empty() {
        let mut list = col().key("files").w_full().gap(6.0);
        for (i, f) in m.files.iter().enumerate() {
            list = list.child(file_row(cx, i, f));
        }
        card.preview = Some(list);
    }
    let mut body = col().key("muqun").w_full().gap(8.0);
    let mut any = false;
    if let Some(cwd) = &m.cwd {
        body = body.child(fact(cx, cx.t("Working directory", "工作目录"), cwd.clone()));
        any = true;
    }
    if let Some(ms) = tv::duration_ms(call) {
        body = body.child(fact(
            cx,
            cx.t("Call duration", "调用耗时"),
            format!("{:.1} s", ms as f32 / 1000.0),
        ));
        any = true;
    }
    if let Some(command) = &m.command {
        body = body.child(
            text(super::cap_lines(command, 6).0)
                .font(k.ty.mono)
                .color(k.c.text_accent)
                .wrap_anywhere()
                .selectable(true)
                .w_full(),
        );
        any = true;
    }
    if let Some(goal) = &m.goal {
        body = body.child(
            text(super::cap_lines(goal, 4).0)
                .font(k.ty.small)
                .color(k.c.text_base)
                .wrap()
                .w_full(),
        );
        any = true;
    }
    if let Some(error) = &m.error {
        body = body.child(
            text(error.clone())
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .max_lines(4)
                .selectable(true)
                .w_full(),
        );
        any = true;
    }
    if let Some(output) = &m.output {
        body = body
            .child(caption(k, cx.t("Output", "输出")))
            .child(mono(cx, "out", output, 16));
        any = true;
    }
    if let Some(note) = &m.note {
        body = body.child(
            text(note.clone())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap(),
        );
        any = true;
    }
    if let Some(n) = m.timeline_count {
        body = body.child(fact(
            cx,
            cx.t("Timeline entries", "时间线条目"),
            n.to_string(),
        ));
        any = true;
    }
    if let Some(v) = &m.gateway_version {
        body = body.child(fact(cx, "Gateway", v.clone()));
        any = true;
    }
    if let Some(v) = &m.agents {
        body = body.child(fact(cx, cx.t("Agents", "代理"), v.clone()));
        any = true;
    }
    if let Some(v) = &m.toolchains {
        body = body.child(fact(cx, cx.t("Toolchains", "工具链"), v.clone()));
        any = true;
    }
    if let Some(code) = m.exit {
        body = body.child(fact(cx, cx.t("Exit code", "退出码"), code.to_string()));
        any = true;
    }
    for (i, machine) in m.machines.iter().enumerate() {
        let state = if m.op != MuqunOp::FleetList {
            ""
        } else if machine.unauthorized {
            cx.t("Permission required", "需要权限")
        } else {
            match machine.online {
                Some(true) => cx.t("Online", "在线"),
                Some(false) => cx.t("Offline", "离线"),
                None => cx.t("Unknown", "未知"),
            }
        };
        let dot = match machine.online {
            Some(true) if !machine.unauthorized => k.c.success.icon,
            _ => k.c.icon_faint,
        };
        body = body.child(
            row()
                .key(format!("m{i}"))
                .w_full()
                .gap(8.0)
                .center_y()
                .child(col().size(7.0, 7.0).radius(3.5).fill(dot))
                .child(
                    text(machine.label.clone())
                        .font(k.ty.small_medium)
                        .color(k.c.text_base),
                )
                .child(
                    text(machine.os.clone().unwrap_or_default())
                        .font(k.ty.small)
                        .color(k.c.text_faint)
                        .grow(1.0),
                )
                .child(text(state).font(k.ty.small).color(k.c.text_muted)),
        );
        any = true;
    }
    if !any && m.files.is_empty() && m.status != MuqunStatus::Calling {
        body = body.child(caption(k, cx.t("No details returned.", "没有返回详情。")));
        any = true;
    }
    if any {
        card.body = Some(body);
    }
    card.build(cx)
}
