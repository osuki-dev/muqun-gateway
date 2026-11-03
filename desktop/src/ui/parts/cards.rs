//! Cards: tool calls, diffs, shell runs, skills, to-do lists, compaction.
//! Each is a 1 px hairline with a 3 px left accent (`parts::card`).

use blinc_core::Color;
use blinc_layout::prelude::*;
use blinc_layout::styled_text::{StyledLine, StyledText, TextSpan};
use blinc_layout::FontFamily;
use blinc_theme::{ColorToken, ThemeState};
use muqun_gateway::agents::{
    AgentErrorInfo, CompactionStatus, TodoItem, TokensUsage, ToolCall, ToolCallStatus,
};
use serde_json::Value;

use blinc_cn::prelude::{spinner, SpinnerSize};

use super::summary::count_lines;
use super::{card, clip_lines, link_button, meta, mono, mono_block, scale, PartCtx};
use crate::model::commands::Command;
use crate::ui::markdown;
use crate::ui::text::{ui_text, MONO_FONT};
use crate::ui::timeline::window::DIFF_PREVIEW_LINES;

/// Lines of input or output an opened tool row shows before "Expand all".
const OUTPUT_LINES: usize = 20;
const INPUT_LINES: usize = 20;
/// Height of a collapsed tool row, border included.
const ROW_HEIGHT: f32 = 36.0;

/// The 3 px bar: mint while running, grey once done, red failed.
fn status_accent(status: ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Pending | ToolCallStatus::Streaming | ToolCallStatus::Running => {
            "mq-accent--running"
        }
        ToolCallStatus::Completed => "mq-accent--done",
        ToolCallStatus::Failed => "mq-accent--failed",
    }
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// The input as short `key: value` lines.
fn input_summary(input: &Value, partial: Option<&str>) -> String {
    match input {
        Value::Object(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let one_line: String = v.lines().next().unwrap_or("").chars().take(240).collect();
                let more = if v.lines().count() > 1 { " …" } else { "" };
                format!("{k}: {one_line}{more}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null | Value::Object(_) => partial.unwrap_or("").to_string(),
        other => other.to_string(),
    }
}

/// The output as text: a string, the text items of `content`, or JSON.
fn output_text(call: &ToolCall) -> String {
    match &call.output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => match &call.content {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|i| i.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        },
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn header_row(glyph: &'static str, glyph_class: &str) -> Div {
    div()
        .class("mq-card-head")
        .w_full()
        .flex_row()
        .items_center()
        .min_w(0.0)
        .child(
            div()
                .class("mq-glyph")
                .class(glyph_class.replace("mq-accent--", "mq-tone--"))
                .flex_shrink_0()
                .child(mono(glyph).no_cursor()),
        )
}

/// A collapsible output section with a line limit and a show-all toggle.
fn output_section(ctx: &PartCtx<'_>, what: &str, output: &str, truncated: bool) -> Div {
    let strings = &ctx.app.strings;
    let all = ctx.is_open(&format!("{what}.all"), false);
    let (shown, more) = if all {
        (output.trim_end().to_string(), 0)
    } else {
        clip_lines(output, OUTPUT_LINES)
    };
    let mut section = div()
        .class("mq-output")
        .w_full()
        .flex_col()
        .child(mono_block(shown));
    if more > 0 || all {
        let label = if all {
            strings.show_less().to_string()
        } else {
            format!("{} · {}", strings.more_lines(more), strings.show_all())
        };
        section = section.child(link_button(label, ctx.toggle(&format!("{what}.all"))));
    }
    if truncated {
        section = section.child(meta("…"));
    }
    section
}

/// The call's input on one line: the field that says most (the command, the
/// path, the pattern) or else the first string.
fn args_summary(call: &ToolCall) -> String {
    const KEYS: &[&str] = &[
        "command",
        "cmd",
        "filePath",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ];
    let line = |v: &str| v.lines().next().unwrap_or("").trim().to_string();
    if let Value::Object(map) = &call.input {
        let named = KEYS
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str));
        let first = || map.values().find_map(Value::as_str);
        if let Some(v) = named.or_else(first) {
            return line(v).chars().take(200).collect();
        }
    }
    match (&call.title, &call.input_partial) {
        (Some(title), _) if title != &call.name => line(title),
        (_, Some(partial)) => line(partial).chars().take(200).collect(),
        _ => String::new(),
    }
}

/// A tool call, one 36 px row: `▸ name  args … duration`, a 3 px bar on the
/// left for the state (mint running with a spinner at the right, grey done,
/// red failed, amber while an approval for it waits). Opened, it shows the
/// input and the output.
pub fn tool(ctx: &PartCtx<'_>, call: &ToolCall) -> Div {
    let strings = &ctx.app.strings;
    let status = call.state;
    let waiting = ctx
        .turn
        .as_ref()
        .is_some_and(|t| t.waiting_on(&ctx.item.id));
    let unfinished = matches!(
        status,
        ToolCallStatus::Pending | ToolCallStatus::Streaming | ToolCallStatus::Running
    );
    // A call still "running" in a turn that has ended was cut off.
    let stale = unfinished && ctx.turn_state().finished();
    let accent = if waiting {
        "mq-accent--warning"
    } else if stale {
        "mq-accent--done"
    } else {
        status_accent(status)
    };
    let open = ctx.is_open("tool", false);
    let args = args_summary(call);
    let mut head = div()
        .id(ctx.key("tool"))
        .class("mq-tool-row")
        .w_full()
        .h(ROW_HEIGHT - 2.0)
        .flex_row()
        .items_center()
        .min_w(0.0)
        .cursor_pointer()
        .child(crate::ui::icons::chevron(open))
        .child(
            div()
                .class("mq-tool-name")
                .flex_shrink_0()
                .child(mono(call.name.clone()).no_cursor()),
        )
        .child(
            div()
                .class("mq-tool-title")
                .flex_1()
                .min_w(0.0)
                .overflow_clip()
                .child(mono(args).size(scale::FS_12).no_cursor()),
        );
    if waiting {
        head = head.child(
            meta(strings.waiting_approval())
                .class("mq-tone--warning")
                .no_cursor(),
        );
    } else if unfinished && !stale {
        head = head.child(spinner().size(SpinnerSize::Small));
    } else {
        if status == ToolCallStatus::Failed {
            head = head.child(
                meta(strings.tool_failed())
                    .class("mq-tone--failed")
                    .no_cursor(),
            );
        }
        if let (Some(ran), Some(done)) = (call.time.ran, call.time.completed) {
            head = head.child(
                meta(duration(done.saturating_sub(ran)))
                    .class("mq-text-subtle")
                    .no_cursor(),
            );
        }
    }
    head = head.on_click(ctx.toggle("tool"));
    let mut body = div().flex_col().class("mq-tool-body").child(head);
    if open {
        let input = input_summary(&call.input, call.input_partial.as_deref());
        if !input.trim().is_empty() {
            let (shown, more) = clip_lines(&input, INPUT_LINES);
            let mut section = div()
                .class("mq-tool-input")
                .w_full()
                .flex_col()
                .child(meta(strings.input()))
                .child(mono_block(shown));
            if more > 0 {
                section = section.child(meta(strings.more_lines(more)));
            }
            body = body.child(section);
        }
        let output = output_text(call);
        if !output.trim().is_empty() {
            body = body
                .child(meta(strings.output()).no_cursor())
                .child(output_section(ctx, "out", &output, call.truncated));
        }
    }
    if let Some(err) = &call.error {
        body = body.child(error_line(err));
    }
    card(accent, body)
}

fn error_line(err: &AgentErrorInfo) -> Div {
    div()
        .class("mq-error-line")
        .w_full()
        .child(mono_block(format!("{}: {}", err.name, err.message)).class("mq-text-error"))
}

/// Unified diff lines, coloured as one monospace rich text.
fn diff_text(diff: &str) -> StyledText {
    let t = ThemeState::get();
    let (add, del, hunk, plain) = (
        t.color(ColorToken::Success),
        t.color(ColorToken::Error),
        t.color(ColorToken::Info),
        t.color(ColorToken::TextSecondary),
    );
    let color = |line: &str| -> Color {
        if line.starts_with("+++") || line.starts_with("---") {
            plain
        } else if line.starts_with('+') {
            add
        } else if line.starts_with('-') {
            del
        } else if line.starts_with("@@") {
            hunk
        } else {
            plain
        }
    };
    StyledText::from_lines(
        diff.lines()
            .map(|line| StyledLine::new(line, vec![TextSpan::colored(0, line.len(), color(line))]))
            .collect(),
    )
}

/// Unified-diff lines that say what changed: the file headers (`diff --git`,
/// `---`, `+++`, mode lines) are the path in the card's header already.
fn body_lines(diff: &str) -> Vec<&str> {
    diff.lines()
        .filter(|l| {
            !(l.starts_with("diff --git")
                || l.starts_with("index ")
                || l.starts_with("--- ")
                || l.starts_with("+++ ")
                || l.starts_with("new file mode")
                || l.starts_with("deleted file mode")
                || l.starts_with("similarity index")
                || l.starts_with("rename "))
        })
        .collect()
}

/// Open `file` for editing: with the editor command from the settings, else
/// the system's opener.
fn open_in_editor(app: &crate::ui::App, file: &str) {
    let path = std::path::Path::new(file);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match app.bindings.header.get().and_then(|h| h.directory.clone()) {
            Some(dir) => std::path::Path::new(&dir).join(file),
            None => path.to_path_buf(),
        }
    };
    let command = app.config.get().general.editor_command;
    let mut words = command.as_deref().unwrap_or("").split_whitespace();
    let Some(program) = words.next() else {
        crate::platform::shell::open_folder(&path);
        return;
    };
    match std::process::Command::new(program)
        .args(words)
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(err) => tracing::warn!(program, "could not open {}: {err}", path.display()),
    }
}

/// A file's diff. Header: the path (mono) and `+n −m` -- both open the diff
/// panel -- then "Open in editor", "Revert" (staged, not applied) and a
/// chevron that folds the card. Body: the unified diff, its first 12 lines
/// until "… N more lines" is clicked.
pub fn diff(ctx: &PartCtx<'_>, file: &str, diff: &str) -> Div {
    let strings = &ctx.app.strings;
    let folded = ctx.is_open("diff.folded", false);
    let all = ctx.is_open("diff.all", false);
    let (adds, dels) = count_lines(diff);
    let lines = body_lines(diff);
    let limit = DIFF_PREVIEW_LINES as usize;
    let more = if all {
        0
    } else {
        lines.len().saturating_sub(limit)
    };
    let shown = lines[..if all {
        lines.len()
    } else {
        lines.len().min(limit)
    }]
        .join("\n");
    let asid = muqun_gateway::agents::AgentSessionId(ctx.asid.to_string());
    let named = !file.is_empty();
    let open_app = ctx.app.clone();
    let open_asid = asid.clone();
    let open_file = named.then(|| file.to_string());
    let title = div()
        .id(ctx.key("diff.title"))
        .class("mq-diff-title")
        .flex_1()
        .min_w(0.0)
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(
            div().min_w(0.0).overflow_clip().flex_shrink().child(
                mono(if named { file } else { "diff" }.to_string())
                    .class("mq-tool-name")
                    .no_cursor(),
            ),
        )
        .child(
            mono(format!("+{adds}"))
                .class("mq-text-success")
                .no_cursor(),
        )
        .child(mono(format!("−{dels}")).class("mq-text-error").no_cursor())
        .on_click(move |_| {
            open_app.send(Command::OpenDiff {
                asid: open_asid.clone(),
                file: open_file.clone(),
            })
        });
    let mut head = div()
        .class("mq-diff-head")
        .w_full()
        .h(ROW_HEIGHT - 2.0)
        .flex_row()
        .items_center()
        .child(title);
    if named {
        let edit_app = ctx.app.clone();
        let edit_file = file.to_string();
        let revert_app = ctx.app.clone();
        let revert_file = file.to_string();
        head = head
            .child(link_button(strings.open_in_editor(), move |_| {
                open_in_editor(&edit_app, &edit_file)
            }))
            .child(
                link_button(strings.revert(), move |_| {
                    revert_app.send(Command::StageRevert {
                        asid: asid.clone(),
                        file: revert_file.clone(),
                    })
                })
                .class("mq-link-button--quiet"),
            );
    }
    head = head.child(
        div()
            .id(ctx.key("diff.fold"))
            .class("mq-chevron-button")
            .cursor_pointer()
            .child(crate::ui::icons::chevron(!folded))
            .on_click(ctx.toggle("diff.folded")),
    );
    let mut body = div().flex_col().class("mq-tool-body").child(head);
    if !folded && !shown.is_empty() {
        body = body.child(
            div().class("mq-diff-body").w_full().child(
                rich_text_styled(diff_text(&shown))
                    .size(scale::FS_12)
                    .line_height(1.5)
                    .font_family(FontFamily::named(MONO_FONT)),
            ),
        );
        if more > 0 {
            body = body.child(link_button(
                format!("… {}", strings.more_lines(more)),
                ctx.toggle("diff.all"),
            ));
        } else if all {
            body = body.child(link_button(strings.show_less(), ctx.toggle("diff.all")));
        }
    }
    card("mq-accent--info", body)
}

/// A to-do list as a checklist.
pub fn todo(ctx: &PartCtx<'_>, items: &[TodoItem]) -> Div {
    let done = items.iter().filter(|i| i.done).count();
    let mut list = div().flex_col().child(
        header_row("☰", "mq-accent--subtle")
            .child(div().flex_1().child(meta(ctx.app.strings.todo_title())))
            .child(meta(format!("{done}/{}", items.len()))),
    );
    for item in items {
        let line = div()
            .class("mq-todo")
            .w_full()
            .flex_row()
            .items_start()
            .child(mono(if item.done { "✓" } else { "○" }).class(if item.done {
                "mq-tone--done"
            } else {
                "mq-tone--subtle"
            }))
            .child(
                div().flex_1().min_w(0.0).child(
                    ui_text(item.text.clone())
                        .size(scale::FS_13)
                        .line_height(1.5)
                        .class(if item.done {
                            "mq-text-subtle"
                        } else {
                            "mq-text"
                        }),
                ),
            );
        list = list.child(line);
    }
    card("mq-accent--subtle", list)
}

/// A compaction boundary: status, tokens and cost.
pub fn compaction(
    ctx: &PartCtx<'_>,
    status: CompactionStatus,
    tokens: Option<&TokensUsage>,
    cost: Option<f64>,
    error: Option<&AgentErrorInfo>,
) -> Div {
    let s = &ctx.app.strings;
    let label = match status {
        CompactionStatus::Started | CompactionStatus::Running => s.compacting(),
        CompactionStatus::Completed => s.compacted(),
        CompactionStatus::Failed => s.compaction_failed(),
    };
    let mut line = div()
        .class("mq-notice")
        .w_full()
        .flex_row()
        .items_center()
        .justify_center()
        .child(meta(label));
    if let Some(t) = tokens {
        line = line
            .child(meta("·"))
            .child(mono(format!("{} in · {} out", t.input, t.output)));
    }
    if let Some(cost) = cost.filter(|c| *c > 0.0) {
        line = line.child(meta("·")).child(mono(format!("${cost:.4}")));
    }
    let mut block = div().flex_col().w_full().child(line);
    if let Some(err) = error {
        block = block.child(error_line(err));
    }
    block
}

/// A skill the agent activated; its text collapsed.
pub fn skill(ctx: &PartCtx<'_>, skill: &str, name: &str, text: &str) -> Div {
    let open = ctx.is_open("skill", false);
    let label = if name.is_empty() { skill } else { name };
    let head = header_row("✦", "mq-accent--subtle")
        .id(ctx.key("skill"))
        .cursor_pointer()
        .child(meta(ctx.app.strings.skill_used()).no_cursor())
        .child(
            div()
                .flex_1()
                .min_w(0.0)
                .child(mono(label.to_string()).no_cursor()),
        )
        .on_click(ctx.toggle("skill"));
    let mut body = div().flex_col().child(head);
    if open && !text.trim().is_empty() {
        body = body.child(
            div()
                .class("mq-card-section")
                .w_full()
                .child(markdown::render(
                    &markdown::parse(text),
                    &markdown::Setting {
                        // The card's hairlines, accent and padding.
                        width: ctx.width - 29.0,
                        ..ctx.setting(true)
                    },
                    &ctx.chrome(&ctx.key("md")),
                )),
        );
    }
    card("mq-accent--subtle", body)
}

/// A shell run: the command, its status and exit code, the output
/// collapsed.
pub fn shell(
    ctx: &PartCtx<'_>,
    command: &str,
    status: &str,
    exit: Option<f64>,
    output: Option<&str>,
    truncated: bool,
) -> Div {
    let failed = exit.is_some_and(|c| c != 0.0) || status == "failed" || status == "error";
    let live = exit.is_none() && matches!(status, "running" | "pending" | "started");
    let stale = live && ctx.turn_state().finished();
    let running = live && !stale;
    let accent = if failed {
        "mq-accent--failed"
    } else if running {
        "mq-accent--running"
    } else {
        "mq-accent--done"
    };
    let open = ctx.is_open("shell", false);
    let mut head = div()
        .id(ctx.key("shell"))
        .class("mq-tool-row")
        .w_full()
        .h(ROW_HEIGHT - 2.0)
        .flex_row()
        .items_center()
        .min_w(0.0)
        .cursor_pointer()
        .child(crate::ui::icons::chevron(open))
        .child(
            div()
                .class("mq-tool-title")
                .flex_1()
                .min_w(0.0)
                .overflow_clip()
                .child(mono(command.lines().next().unwrap_or("").to_string()).no_cursor()),
        );
    if running {
        head = head.child(spinner().size(SpinnerSize::Small));
    } else if let Some(code) = exit.filter(|c| *c != 0.0) {
        head = head.child(
            meta(ctx.app.strings.exit_code(code))
                .class("mq-tone--failed")
                .no_cursor(),
        );
    }
    let mut body = div()
        .flex_col()
        .class("mq-tool-body")
        .child(head.on_click(ctx.toggle("shell")));
    if open {
        if command.lines().count() > 1 {
            body = body.child(
                div()
                    .class("mq-tool-input")
                    .w_full()
                    .child(mono_block(command.to_string())),
            );
        }
        if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
            body = body.child(output_section(ctx, "out", out, truncated));
        }
    }
    card(accent, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_summarise_to_one_line_per_key() {
        let input = serde_json::json!({"command": "ls -la\nwc -l", "timeout": 30});
        assert_eq!(
            input_summary(&input, None),
            "command: ls -la …\ntimeout: 30"
        );
        assert_eq!(input_summary(&Value::Null, Some("{\"pa")), "{\"pa");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(420), "420ms");
        assert_eq!(duration(4200), "4.2s");
        assert_eq!(duration(125_000), "2m 5s");
    }
}
