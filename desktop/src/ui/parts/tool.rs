//! One tool call, drawn as what it is (App `agent-tool-card.tsx`): a card
//! per family, each with the header, chips and body the App gives it.

use muqun_gateway::agents::{AgentPart, AgentSessionId, TimelineItem, ToolCall};
use rune::{col, icon, row, text, Cursor, El, Icon};
use rune_kit::{Button, ButtonVariant, ControlSize};
use serde_json::{Map, Value};

use super::{
    caption, chip, code, files, glyphs, images, mono, more_count, prose, CardState, Tcx, ToolCard,
};
use crate::model::commands::Command;
use crate::model::tool_view::{self as tv, Output, ToolKind};
use crate::ui::UiState;

/// Lines of output a card shows before "N more lines".
const OUTPUT_LINES: usize = 24;

fn verb(cx: &Tcx, kind: ToolKind, call: &ToolCall) -> String {
    let w = |en, zh| cx.t(en, zh).to_string();
    match kind {
        ToolKind::Shell => w("Run", "运行"),
        ToolKind::Read => w("Read", "读取"),
        ToolKind::Edit => w("Edit", "编辑"),
        ToolKind::Write => w("Write", "写入"),
        ToolKind::Patch => w("Patch", "补丁"),
        ToolKind::Glob => w("Find files", "查找文件"),
        ToolKind::Grep => w("Search", "搜索"),
        ToolKind::Search => w("Search code", "搜索代码"),
        ToolKind::Web => {
            let n = call.name.to_ascii_lowercase();
            if n.contains("search") {
                w("Web search", "网页搜索")
            } else {
                w("Fetch", "获取")
            }
        }
        ToolKind::Subagent => w("Subagent", "子代理"),
        ToolKind::Skill => w("Skill", "技能"),
        ToolKind::Todo => w("Tasks", "任务"),
        ToolKind::Question => w("Question", "提问"),
        ToolKind::Execute => w("Code", "代码"),
        ToolKind::Browser => w("Browser", "浏览器"),
        ToolKind::Mcp => mcp_name(&call.name),
    }
}

/// `mcp__server__tool` as `server · tool`; anything else as it is.
fn mcp_name(name: &str) -> String {
    match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
        Some((server, tool)) => format!("{server} · {tool}"),
        None => name.to_string(),
    }
}

fn glyph(kind: ToolKind) -> &'static Icon {
    match kind {
        ToolKind::Shell => &glyphs::TERMINAL,
        ToolKind::Read => &glyphs::DOCUMENT,
        ToolKind::Edit | ToolKind::Write | ToolKind::Patch => &glyphs::EDIT_FILE,
        ToolKind::Glob => &glyphs::FOLDER,
        ToolKind::Grep | ToolKind::Search => &glyphs::SEARCH,
        ToolKind::Web | ToolKind::Browser => &glyphs::NETWORK,
        ToolKind::Subagent => &glyphs::AGENT,
        ToolKind::Skill => &glyphs::WORKFLOW,
        ToolKind::Todo => &glyphs::TASK_LIST,
        ToolKind::Question => &glyphs::CHOICE,
        ToolKind::Execute => &glyphs::CODE,
        ToolKind::Mcp => &glyphs::TOOL,
    }
}

/// The card's state: a call the stopped turn left pending says Stopped.
fn card_state(cx: &Tcx, call: &ToolCall, failed: bool) -> CardState {
    if tv::pending(call.state) {
        if cx.stopped() {
            CardState::Stopped
        } else {
            CardState::Running
        }
    } else if failed || call.state == muqun_gateway::agents::ToolCallStatus::Failed {
        CardState::Failed
    } else {
        CardState::Done
    }
}

fn error_line(cx: &Tcx, call: &ToolCall) -> Option<String> {
    let message = call.error.as_ref()?.message.trim();
    if message.is_empty() {
        return None;
    }
    Some(if tv::declined_by_user(message) {
        cx.t("Denied by you", "已被你拒绝").to_string()
    } else {
        message.to_string()
    })
}

fn seconds(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f32 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

pub fn card(cx: &Tcx, item: &TimelineItem, call: &ToolCall) -> El {
    if let Some(m) = tv::muqun(call) {
        return super::muqun::card(cx, item, call, &m);
    }
    let rec = tv::input_record(call);
    let mut kind = tv::classify(&call.name);
    // ACP names a shell call by its category, `execute`.
    if kind == ToolKind::Execute && rec.as_ref().is_some_and(|r| r.contains_key("command")) {
        kind = ToolKind::Shell;
    }
    let out = tv::output_text(call);
    let target = tv::target(kind, rec.as_ref());
    let caption_line = tv::caption(kind, rec.as_ref());
    let id = item.id.as_str();
    let mut card = ToolCard::new(id, glyph(kind), verb(cx, kind, call));
    card.error = error_line(cx, call);
    let mut failed = false;

    // Header title and caption (App `headerTitle` / `headerCaption`).
    match kind {
        ToolKind::Read | ToolKind::Edit | ToolKind::Write | ToolKind::Patch => {
            card.title = tv::basename(&target).to_string();
            card.caption = if kind == ToolKind::Patch {
                tv::dirname(&target).to_string()
            } else {
                caption_line.clone()
            };
        }
        ToolKind::Web => match tv::split_url(&target) {
            Some((host, path)) => {
                card.title = host;
                card.caption = path;
            }
            None => card.title = target.clone(),
        },
        _ => {
            card.title = if target.is_empty() {
                call.title.clone().unwrap_or_default()
            } else {
                target.clone()
            };
            card.caption = caption_line.clone();
        }
    }
    if card.title.is_empty() {
        card.title = call.title.clone().unwrap_or_default();
    }
    // A path as a title reads as its file name, over its folder.
    if (card.title.starts_with('/') || card.title.starts_with("~/")) && card.caption.is_empty() {
        card.caption = tv::dirname(&card.title).to_string();
        card.title = tv::basename(&card.title).to_string();
    }

    if let Some(ms) = tv::sleep_ms(call, rec.as_ref()) {
        card.glyph = &glyphs::TIME;
        card.verb = format!("{} · {}", cx.t("Wait", "等待"), seconds(ms));
        card.title.clear();
        card.state = card_state(cx, call, false);
        card.body = output_body(cx, &out);
        return card.build(cx);
    }

    match kind {
        ToolKind::Shell => {
            let exit = tv::shell_exit(call, &out);
            let timed_out = tv::shell_timed_out(call);
            if let Some(code) = exit {
                let color = if code == 0 {
                    cx.k.c.success.fg
                } else {
                    cx.k.c.danger.fg
                };
                card.chips.push(chip(cx.k, format!("exit {code}"), color));
            }
            if timed_out {
                card.chips
                    .push(chip(cx.k, cx.t("timed out", "已超时"), cx.k.c.warning.fg));
            }
            failed = exit.is_some_and(|c| c != 0) || timed_out;
            let mut body = col().key("shell").w_full().gap(8.0);
            if !target.is_empty() {
                body = body.child(command_line(cx, &target));
            }
            if !out.stdout.trim().is_empty() {
                body = body.child(mono(cx, "out", &out.stdout, OUTPUT_LINES));
            }
            card.body = Some(body);
            let detached = call.background
                || rec
                    .as_ref()
                    .is_some_and(|r| r.get("background") == Some(&Value::Bool(true)));
            if tv::pending(call.state) && !cx.stopped() && !detached {
                let asid = cx.asid.to_string();
                card.actions.push(
                    Button::new(cx.t("Run in background", "转入后台运行"))
                        .variant(ButtonVariant::Ghost)
                        .control_size(ControlSize::Small)
                        .on_press(move |s: &mut UiState| {
                            s.send(Command::BackgroundSession {
                                asid: AgentSessionId(asid.clone()),
                            })
                        })
                        .build(cx.k),
                );
            }
        }
        ToolKind::Read => {
            card.body = files::read_body(cx, call, &target);
        }
        ToolKind::Edit | ToolKind::Write | ToolKind::Patch => {
            let (body, chips) = files::change_body(cx, item, call, kind);
            card.body = body;
            card.chips = chips;
        }
        ToolKind::Glob => {
            let groups = tv::group_paths(&out.stdout);
            count_chip(
                cx,
                call,
                &mut card,
                groups.iter().map(|g| g.files.len()).sum(),
            );
            card.body = if groups.is_empty() {
                output_body(cx, &out)
            } else {
                Some(glob_body(cx, &groups))
            };
        }
        ToolKind::Grep => {
            let groups = tv::group_grep(&out.stdout);
            count_chip(
                cx,
                call,
                &mut card,
                groups.iter().map(|g| g.lines.len()).sum(),
            );
            card.body = if groups.is_empty() {
                output_body(cx, &out)
            } else {
                Some(grep_body(cx, &groups))
            };
        }
        ToolKind::Search => {
            count_chip(cx, call, &mut card, 0);
            card.body = output_body(cx, &out);
        }
        ToolKind::Web => {
            if let Some(t) = tv::meta_str(call, &["contentType", "content_type", "mime"]) {
                let t = t.split(';').next().unwrap_or(t).trim().to_string();
                card.chips.push(chip(cx.k, t, cx.k.c.text_muted));
            }
            if let Some(p) = tv::meta_str(call, &["provider", "engine"]) {
                card.chips
                    .push(chip(cx.k, p.to_string(), cx.k.c.text_muted));
            }
            if !out.stdout.trim().is_empty() {
                let (shown, hidden) = super::cap_lines(&out.stdout, 80);
                let mut body = col()
                    .key("web")
                    .w_full()
                    .gap(4.0)
                    .child(prose(cx, "web", &shown));
                if hidden > 0 {
                    body = body.child(super::more_lines(cx, hidden));
                }
                card.body = Some(body);
            }
        }
        ToolKind::Subagent => subagent(cx, call, rec.as_ref(), &out, &mut card),
        ToolKind::Skill => {
            if let Some(name) = tv::meta_str(call, &["name", "title"]) {
                card.title = name.to_string();
            }
            if let Some(dir) = tv::meta_str(call, &["directory", "dir", "path"]) {
                card.caption = dir.to_string();
            }
            let description = rec
                .as_ref()
                .and_then(|r| r.get("description"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| out.stdout.clone());
            if !description.trim().is_empty() {
                let (shown, hidden) = super::cap_lines(&description, 60);
                let mut body = col()
                    .key("skill")
                    .w_full()
                    .child(prose(cx, "skill", &shown));
                if hidden > 0 {
                    body = body.child(super::more_lines(cx, hidden));
                }
                card.body = Some(body);
            }
        }
        ToolKind::Todo => {
            let items = tv::todo_items(rec.as_ref());
            if !items.is_empty() {
                return todo_card(cx, id, &items);
            }
            card.body = generic_body(cx, call, &out);
        }
        ToolKind::Question => {
            super::question::question_card(cx, item, call, rec.as_ref(), &out, &mut card)
        }
        ToolKind::Execute => {
            failed = tv::execute_errored(call);
            if failed {
                card.chips
                    .push(chip(cx.k, cx.t("error", "出错"), cx.k.c.danger.fg));
            }
            let mut body = col().key("exec").w_full().gap(8.0);
            if let Some(code_text) = rec
                .as_ref()
                .and_then(|r| r.get("code"))
                .and_then(Value::as_str)
            {
                body = body.child(code(cx, "code", code_text, Some("javascript"), 60));
            }
            let calls = tv::execute_calls(call);
            for (i, c) in calls.iter().take(12).enumerate() {
                body = body.child(execute_row(cx, i, c));
            }
            if calls.len() > 12 {
                body = body.child(more_count(cx, calls.len() - 12));
            }
            if !out.stdout.trim().is_empty() {
                body = body.child(mono(cx, "out", &out.stdout, OUTPUT_LINES));
            }
            card.body = Some(body);
        }
        ToolKind::Browser | ToolKind::Mcp => {
            card.body = generic_body(cx, call, &out);
        }
    }

    // While the input is still arriving, its arguments are all there is.
    if tv::pending(call.state) && !cx.stopped() && !(kind == ToolKind::Shell && !target.is_empty())
    {
        if let Some(line) = rec
            .as_ref()
            .map(tv::argument_line)
            .filter(|l| !l.is_empty())
        {
            card.preview = Some(
                text(line)
                    .font(cx.k.ty.mono)
                    .color(cx.k.c.text_faint)
                    .max_lines(2)
                    .w_full(),
            );
        }
    }
    if let Some(gallery) = images::gallery(
        cx,
        &format!("tool-{id}"),
        &images::tool_image_paths(cx.s, cx.asid, call),
    ) {
        card.preview = Some(gallery);
    }
    if let Some(ms) = tv::duration_ms(call).filter(|ms| *ms >= 1000) {
        if card.chips.is_empty() || kind == ToolKind::Shell {
            card.chips.push(
                text(seconds(ms))
                    .font(cx.k.ty.small)
                    .color(cx.k.c.text_faint)
                    .single_line(),
            );
        }
    }
    if out.truncated && card.body.is_some() {
        card.chips
            .push(chip(cx.k, cx.t("Truncated", "已截断"), cx.k.c.warning.fg));
    }
    card.state = card_state(cx, call, failed);
    card.build(cx)
}

fn count_chip(cx: &Tcx, call: &ToolCall, card: &mut ToolCard, counted: usize) {
    let n = tv::result_count(call).map_or(counted, |n| n as usize);
    if n == 0 && tv::pending(call.state) {
        return;
    }
    let label = if cx.zh() {
        format!("{n} 个结果")
    } else if n == 1 {
        "1 result".to_string()
    } else {
        format!("{n} results")
    };
    card.chips.push(chip(cx.k, label, cx.k.c.text_muted));
}

fn output_body(cx: &Tcx, out: &Output) -> Option<El> {
    (!out.stdout.trim().is_empty()).then(|| mono(cx, "out", &out.stdout, OUTPUT_LINES))
}

fn command_line(cx: &Tcx, command: &str) -> El {
    row()
        .key("cmd")
        .w_full()
        .gap(8.0)
        .child(text("$").font(cx.k.ty.mono).color(cx.k.c.text_faint))
        .child(
            text(command.to_string())
                .font(cx.k.ty.mono)
                .color(cx.k.c.text_accent)
                .wrap_anywhere()
                .selectable(true)
                .selection_color(cx.k.selection())
                .shrink()
                .grow(1.0),
        )
}

/// Unknown tools: their arguments (as JSON) and what they returned.
fn generic_body(cx: &Tcx, call: &ToolCall, out: &Output) -> Option<El> {
    let mut body = col().key("generic").w_full().gap(8.0);
    let mut any = false;
    let input = match &call.input {
        Value::Object(m) if !m.is_empty() => Some(Value::Object(m.clone())),
        Value::Array(a) if !a.is_empty() => Some(Value::Array(a.clone())),
        _ => None,
    };
    if let Some(input) = input {
        let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
        body = body.child(caption(cx.k, cx.t("Input", "输入")));
        body = body.child(code(cx, "input", &pretty, Some("json"), 40));
        any = true;
    }
    if !out.stdout.trim().is_empty() {
        body = body.child(caption(cx.k, cx.t("Output", "输出")));
        let marked = ["\n#", "\n- ", "\n* ", "```", "|---", "**"]
            .iter()
            .any(|m| out.stdout.contains(m))
            || out.stdout.starts_with('#');
        body = body.child(if marked {
            let (shown, hidden) = super::cap_lines(&out.stdout, 80);
            let mut b = col().key("md").w_full().child(prose(cx, "out", &shown));
            if hidden > 0 {
                b = b.child(super::more_lines(cx, hidden));
            }
            b
        } else {
            mono(cx, "out", &out.stdout, OUTPUT_LINES)
        });
        any = true;
    }
    any.then_some(body)
}

fn glob_body(cx: &Tcx, groups: &[tv::PathGroup]) -> El {
    let k = cx.k;
    let mut body = col().key("glob").w_full().gap(8.0);
    for (gi, group) in groups.iter().take(8).enumerate() {
        let mut block = col().key(format!("g{gi}")).w_full().gap(2.0).child(
            text(if group.directory.is_empty() {
                "/".to_string()
            } else {
                group.directory.clone()
            })
            .font(k.ty.small_medium)
            .color(k.c.text_muted)
            .single_line()
            .truncate_head()
            .w_full(),
        );
        for (fi, name) in group.files.iter().take(12).enumerate() {
            let path = if group.directory.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", group.directory)
            };
            block = block.child(
                k.interactive(
                    row()
                        .key(format!("f{fi}"))
                        .w_full()
                        .gap(6.0)
                        .px(4.0)
                        .py(1.0)
                        .radius(k.radius.sm)
                        .center_y()
                        .child(icon(&glyphs::DOCUMENT, 11.0).color(k.c.icon_faint))
                        .child(
                            text(name.clone())
                                .font(k.ty.mono)
                                .color(k.c.text_base)
                                .single_line()
                                .truncate_middle()
                                .shrink(),
                        ),
                )
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut UiState| s.open_file(&path)),
            );
        }
        if group.files.len() > 12 {
            block = block.child(more_count(cx, group.files.len() - 12));
        }
        body = body.child(block);
    }
    if groups.len() > 8 {
        body = body.child(more_count(cx, groups.len() - 8));
    }
    body
}

fn grep_body(cx: &Tcx, groups: &[tv::GrepFile]) -> El {
    let k = cx.k;
    let mut body = col().key("grep").w_full().gap(8.0);
    for (gi, group) in groups.iter().take(12).enumerate() {
        let path = group.file.clone();
        let mut block = col().key(format!("g{gi}")).w_full().gap(1.0).child(
            k.interactive(
                text(group.file.clone())
                    .font(k.ty.small_medium)
                    .color(k.c.text_base)
                    .single_line()
                    .truncate_head()
                    .w_full(),
            )
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut UiState| s.open_file(&path)),
        );
        for (li, (n, line)) in group.lines.iter().take(8).enumerate() {
            block = block.child(
                row()
                    .key(format!("l{li}"))
                    .w_full()
                    .gap(8.0)
                    .child(
                        text(n.map(|n| n.to_string()).unwrap_or_default())
                            .font(k.ty.mono)
                            .color(k.c.text_faint)
                            .w(36.0)
                            .single_line(),
                    )
                    .child(
                        text(line.clone())
                            .font(k.ty.mono)
                            .color(k.c.text_muted)
                            .single_line()
                            .truncate()
                            .shrink()
                            .grow(1.0),
                    ),
            );
        }
        if group.lines.len() > 8 {
            block = block.child(more_count(cx, group.lines.len() - 8));
        }
        body = body.child(block);
    }
    if groups.len() > 12 {
        body = body.child(more_count(cx, groups.len() - 12));
    }
    body
}

fn execute_row(cx: &Tcx, i: usize, c: &tv::ExecuteCall) -> El {
    let k = cx.k;
    let color = match c.ok {
        Some(false) => k.c.danger.icon,
        None => k.c.warning.icon,
        Some(true) => k.c.success.icon,
    };
    row()
        .key(format!("call{i}"))
        .w_full()
        .gap(8.0)
        .center_y()
        .child(col().size(6.0, 6.0).radius(3.0).fill(color))
        .child(
            text(c.name.clone())
                .font(k.ty.small_medium)
                .color(if c.ok == Some(false) {
                    k.c.danger.fg
                } else {
                    k.c.text_base
                }),
        )
        .child(
            text(c.input.clone())
                .font(k.ty.mono)
                .color(k.c.text_faint)
                .single_line()
                .truncate()
                .shrink(),
        )
}

/// App `subagentPresentation` and its inline body.
fn subagent(
    cx: &Tcx,
    call: &ToolCall,
    rec: Option<&Map<String, Value>>,
    out: &Output,
    card: &mut ToolCard,
) {
    let k = cx.k;
    let child = call
        .child_session_id
        .as_deref()
        .and_then(|c| cx.s.session_by_id(c));
    let get = |key: &str| {
        rec.and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let identity = ["agent", "agentID", "agent_id", "subagent_type"]
        .iter()
        .find_map(|k| get(k))
        .map(str::to_string)
        .or_else(|| tv::meta_str(call, &["agentPath"]).map(str::to_string));
    let summary = get("description")
        .map(str::to_string)
        .or_else(|| get("prompt").map(|p| tv::first_line(p).to_string()))
        .or_else(|| child.map(|c| c.title.clone()).filter(|t| !t.is_empty()));
    card.title = summary
        .clone()
        .or_else(|| call.title.clone())
        .unwrap_or_default();
    card.caption = identity.unwrap_or_default();
    let (result, envelope_state) = tv::strip_subagent_envelope(&out.stdout);
    let state = child
        .map(|c| format!("{:?}", c.status).to_ascii_lowercase())
        .or_else(|| tv::meta_str(call, &["status"]).map(str::to_string))
        .or(envelope_state);
    if let Some(state) = state.as_deref() {
        let (label, color) = match state {
            "busy" | "working" | "starting" | "retry" => {
                (cx.t("Working", "工作中"), k.c.warning.fg)
            }
            "idle" => (cx.t("Idle", "空闲"), k.c.text_muted),
            "completed" => (cx.t("Completed", "已完成"), k.c.success.fg),
            "failed" | "error" => (cx.t("Failed", "失败"), k.c.danger.fg),
            "interrupted" | "shutdown" | "stopped" => (cx.t("Stopped", "已停止"), k.c.text_muted),
            "blocked" => (cx.t("Needs input", "需要输入"), k.c.warning.fg),
            _ => ("", k.c.text_muted),
        };
        if !label.is_empty() {
            card.chips.push(chip(k, label, color));
        }
    }
    let mut body = col().key("sub").w_full().gap(8.0);
    if !result.trim().is_empty() {
        let (shown, hidden) = super::cap_lines(&result, 80);
        body = body.child(prose(cx, "result", &shown));
        if hidden > 0 {
            body = body.child(super::more_lines(cx, hidden));
        }
    } else {
        if let Some(prompt) = get("prompt") {
            body = body
                .child(caption(k, cx.t("Prompt", "提示词")))
                .child(prose(cx, "prompt", prompt));
        }
        body = body.child(caption(
            k,
            if call.child_session_id.is_some() {
                cx.t(
                    "This activity has no result attached. Open the session to read the conversation.",
                    "此活动记录未附带结果。打开会话查看对话。",
                )
            } else {
                cx.t("This activity has no result attached.", "此活动记录未附带结果")
            },
        ));
    }
    card.body = Some(body);
    if let Some(child_id) = call.child_session_id.clone() {
        card.actions.push(
            Button::new(cx.t("Open session", "打开会话"))
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .on_press(move |s: &mut UiState| s.open_session(&child_id))
                .build(k),
        );
    }
}

/// A checklist (a todo tool or a `Todo` part): "N of M done" over the items.
pub fn todo_card(cx: &Tcx, id: &str, items: &[(String, bool)]) -> El {
    let k = cx.k;
    let done = items.iter().filter(|(_, d)| *d).count();
    let mut card = ToolCard::new(id, &glyphs::TASK_LIST, cx.t("Tasks", "任务"));
    card.title = if cx.zh() {
        format!("已完成 {done}/{}", items.len())
    } else {
        format!("{done} of {} done", items.len())
    };
    card.open_default = true;
    let mut list = col().key("todo").w_full().gap(4.0);
    for (i, (label, finished)) in items.iter().enumerate() {
        list = list.child(
            row()
                .key(format!("todo-{i}"))
                .w_full()
                .gap(8.0)
                .child(
                    icon(
                        if *finished {
                            &glyphs::COMPLETED
                        } else {
                            &glyphs::CHOICE
                        },
                        14.0,
                    )
                    .color(if *finished {
                        k.c.success.icon
                    } else {
                        k.c.icon_faint
                    })
                    .mt(3.0),
                )
                .child(
                    {
                        let line = text(label.clone());
                        if *finished {
                            line.strike(0, label.len(), None)
                        } else {
                            line
                        }
                    }
                    .font(k.ty.ui)
                    .color(if *finished {
                        k.c.text_muted
                    } else {
                        k.c.text_base
                    })
                    .wrap()
                    .shrink()
                    .grow(1.0),
                ),
        );
    }
    card.body = Some(list);
    card.build(cx)
}

/// A shell run outside the tool loop (`Session.Message.Shell`).
pub fn shell_part(cx: &Tcx, item: &TimelineItem) -> El {
    let AgentPart::Shell {
        command,
        status,
        exit,
        output,
        truncated,
        ..
    } = &item.part
    else {
        return col();
    };
    let k = cx.k;
    let mut card = ToolCard::new(&item.id, &glyphs::TERMINAL, cx.t("Shell", "终端"));
    card.title = command.clone();
    if let Some(code) = exit {
        let color = if *code == 0.0 {
            k.c.success.fg
        } else {
            k.c.danger.fg
        };
        card.chips.push(chip(k, format!("exit {code}"), color));
    }
    if *truncated {
        card.chips
            .push(chip(k, cx.t("Truncated", "已截断"), k.c.warning.fg));
    }
    card.state = match status.as_str() {
        "running" | "pending" if !cx.stopped() => CardState::Running,
        "running" | "pending" => CardState::Stopped,
        _ if exit.is_some_and(|e| e != 0.0) => CardState::Failed,
        _ => CardState::Done,
    };
    let mut body = col()
        .key("shell")
        .w_full()
        .gap(8.0)
        .child(command_line(cx, command));
    if let Some(out) = output.as_ref().filter(|o| !o.trim().is_empty()) {
        body = body.child(mono(cx, "out", out, OUTPUT_LINES));
    }
    card.body = Some(body);
    card.build(cx)
}
