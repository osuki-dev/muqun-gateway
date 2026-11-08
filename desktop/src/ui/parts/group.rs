//! A run of tool activity, folded: one summary line (`Read 6 files · ran
//! 12 commands · 1 failed`) that opens to the calls' dense rows. While a
//! call runs, the line names it beside a spinner.

use muqun_gateway::agents::{AgentPart, TimelineItem};
use rune::{col, icon, row, text, Cursor, El};
use rune_kit::Spinner;

use super::{glyphs, Tcx, ROW_H};
use crate::model::activity::{self, Summary};
use crate::model::tool_view as tv;
use crate::ui::copy::localize;
use crate::ui::timeline::group_key;
use crate::ui::UiState;

/// `n` of a thing, in the owner's language: `6 files` / `6 个文件`.
fn count(cx: &Tcx, n: usize, en_one: &str, en_many: &str, zh: &str) -> String {
    if cx.zh() {
        localize(cx.lang, format!("{n} {zh}"))
    } else if n == 1 {
        format!("1 {en_one}")
    } else {
        format!("{n} {en_many}")
    }
}

/// The summary's phrases, in the order a reader scans for them.
pub fn phrases(cx: &Tcx, s: &Summary) -> Vec<String> {
    let zh = cx.zh();
    let mut out = Vec::new();
    let mut push = |n: usize, verb_en: &str, verb_zh: &str, noun: String| {
        if n > 0 {
            out.push(if zh {
                format!("{}{noun}", localize(cx.lang, verb_zh.to_string()))
            } else {
                format!("{verb_en} {noun}")
            });
        }
    };
    push(
        s.read,
        "Read",
        "读取 ",
        count(cx, s.read, "file", "files", "个文件"),
    );
    push(
        s.edited,
        "edited",
        "编辑 ",
        count(cx, s.edited, "file", "files", "个文件"),
    );
    push(
        s.ran,
        "ran",
        "运行 ",
        count(cx, s.ran, "command", "commands", "条命令"),
    );
    push(
        s.searched,
        "searched",
        "搜索 ",
        count(cx, s.searched, "time", "times", "次"),
    );
    push(
        s.web,
        "browsed",
        "浏览 ",
        count(cx, s.web, "page", "pages", "个网页"),
    );
    push(
        s.subagents,
        "started",
        "启动 ",
        count(cx, s.subagents, "subagent", "subagents", "个子代理"),
    );
    push(
        s.other,
        "used",
        "使用 ",
        count(cx, s.other, "tool", "tools", "个工具"),
    );
    // Sentence case: the line starts with a capital whatever comes first.
    if let Some(first) = out.first_mut() {
        let mut chars = first.chars();
        if let Some(c) = chars.next() {
            *first = c.to_uppercase().chain(chars).collect();
        }
    }
    out
}

/// What the running call is doing, for the line while it works.
fn step(cx: &Tcx, item: &TimelineItem) -> String {
    match &item.part {
        AgentPart::Tool(call) => {
            let kind = activity::tool_kind(call);
            let rec = tv::input_record(call);
            let mut target = tv::target(kind, rec.as_ref());
            if kind == tv::ToolKind::Shell {
                target = tv::display_command(&target);
            } else if target.starts_with('/') || target.starts_with("~/") {
                target = tv::basename(&target).to_string();
            }
            let target = tv::first_line(&target).to_string();
            let verb = super::tool::verb_of(cx, kind, call);
            if target.is_empty() {
                verb
            } else {
                format!("{verb} {target}")
            }
        }
        AgentPart::Shell { command, .. } => {
            format!("{} {}", cx.t("Run", "运行"), tv::first_line(command))
        }
        _ => cx.t("Working", "工作中").to_string(),
    }
}

pub fn view(cx: &Tcx, items: &[TimelineItem]) -> El {
    let k = cx.k;
    let key = group_key(&items[0]);
    let open = cx.is_open(&key, false);
    let summary = activity::summarize(items);
    let running = summary.running.filter(|_| !cx.stopped()).map(|i| &items[i]);
    let selected = cx.selected(&key);
    let turn = if open {
        0.0
    } else {
        -std::f32::consts::FRAC_PI_2
    };
    let mut line = row()
        .key("summary")
        .w_full()
        .h(ROW_H)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(k.radius.sm)
        .child(
            icon(&glyphs::CHEVRON, 11.0)
                .key("chevron")
                .color(k.c.icon_faint)
                .rotate(turn)
                .a11y_hidden(),
        );
    let words = phrases(cx, &summary).join(" · ");
    // The counts hold their room; the running step gives way first.
    let mut said = row().key("said").center_y().child(
        text(words)
            .key("words")
            .font(k.ty.small_medium)
            .color(k.c.text_muted)
            .single_line(),
    );
    if summary.failed > 0 {
        let failed = if cx.zh() {
            localize(cx.lang, format!("{} 个失败", summary.failed))
        } else {
            format!("{} failed", summary.failed)
        };
        said = said.child(
            text(format!(" · {failed}"))
                .key("failed")
                .font(k.ty.small_medium)
                .color(k.c.danger.fg)
                .single_line(),
        );
    }
    line = line.child(said);
    if let Some(item) = running {
        line = line
            .child(text("·").font(k.ty.small).color(k.c.text_faint))
            .child(Spinner::new().size(11.0).build(k).key("spin"))
            .child(
                text(step(cx, item))
                    .key("step")
                    .font(k.ty.mono)
                    .color(k.c.text_faint)
                    .single_line()
                    .truncate()
                    .shrink(),
            );
    }
    line = line.child(col().grow(1.0));
    if selected {
        line = line.fill(k.c.surface_raised_hover);
    }
    let (asid, toggle) = (cx.asid.to_string(), key.clone());
    let line = k
        .interactive(line)
        .cursor(Cursor::Pointer)
        .role("button")
        .expanded(open)
        .on_press_tx(move |s: &mut UiState, tx: &mut rune::Tx| s.toggle_group(&asid, &toggle, tx));
    let mut block = col()
        .key(format!("group-{key}"))
        .w_full()
        .gap(1.0)
        .child(line);
    if open {
        // The calls, on a hairline rail under the chevron.
        let mut list = col().key("calls").w_full().gap(1.0).grow(1.0).shrink();
        for item in items.iter().filter(|i| !activity::invisible(i)) {
            list = list.child(super::part(cx, item).key(format!("in-{}", item.id)));
        }
        let mut rail = row()
            .key("rail")
            .w_full()
            .child(
                col()
                    .key("line")
                    .w(1.0)
                    .h_full()
                    .ml(11.0)
                    .mr(6.0)
                    .fill(k.c.border_weak),
            )
            .child(list);
        if !cx.reduced_motion() {
            rail = rail.transition(rune::Transition::opacity());
        }
        block = block.child(rail);
    }
    block
}
