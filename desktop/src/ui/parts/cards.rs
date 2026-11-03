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
use super::{card, clip_lines, link_button, media, meta, mono, mono_block, scale, PartCtx};
use crate::model::commands::Command;
use crate::ui::markdown;
use crate::ui::text::{ui_text, MONO_FONT};
use crate::ui::timeline::window::DIFF_PREVIEW_LINES;

/// Lines of input or output an opened tool row shows before "Expand all".
const OUTPUT_LINES: usize = 20;
const INPUT_LINES: usize = 20;
/// Height of a collapsed tool row, border included.
const ROW_HEIGHT: f32 = 36.0;

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Cheap brace check for auto-open; does not parse.
fn looks_like_json_object(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('{') && t.ends_with('}')) || (t.starts_with('[') && t.ends_with(']'))
}

/// Pretty JSON when `value` is an object or array.
fn pretty_structured(value: &Value) -> Option<String> {
    matches!(value, Value::Object(_) | Value::Array(_))
        .then(|| serde_json::to_string_pretty(value).ok())
        .flatten()
}

/// Text for display, and whether it should render as a JSON code block.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Payload {
    text: String,
    json: bool,
}

impl Payload {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            json: false,
        }
    }

    fn json(text: String) -> Self {
        Self { text, json: true }
    }

    /// Prefer structured pretty-print; otherwise try to parse a JSON string.
    fn from_value(value: &Value) -> Self {
        if let Some(pretty) = pretty_structured(value) {
            return Self::json(pretty);
        }
        match value {
            Value::String(s) => Self::from_text(s),
            Value::Null => Self::plain(String::new()),
            other => Self::plain(other.to_string()),
        }
    }

    /// Compact `key: value` for tiny flat objects; pretty JSON once nested
    /// or multi-line values appear.
    fn from_input(input: &Value, partial: Option<&str>) -> Self {
        match input {
            Value::Object(map) if !map.is_empty() => {
                if map.values().any(|v| {
                    matches!(v, Value::Object(_) | Value::Array(_))
                        || matches!(v, Value::String(s) if s.contains('\n') || s.len() > 120)
                }) {
                    return pretty_structured(input)
                        .map(Self::json)
                        .unwrap_or_else(|| Self::plain(input.to_string()));
                }
                let lines: Vec<String> = map
                    .iter()
                    .map(|(k, v)| {
                        let v = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        format!("{k}: {}", v.chars().take(240).collect::<String>())
                    })
                    .collect();
                Self::plain(lines.join("\n"))
            }
            Value::Array(_) => pretty_structured(input)
                .map(Self::json)
                .unwrap_or_else(|| Self::plain(input.to_string())),
            Value::Null | Value::Object(_) => Self::from_text(partial.unwrap_or("")),
            other => Self::from_value(other),
        }
    }

    fn from_text(text: &str) -> Self {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Self::plain(String::new());
        }
        if looks_like_json_object(trimmed) {
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                if let Some(pretty) = pretty_structured(&value) {
                    return Self::json(pretty);
                }
            }
        }
        Self::plain(text.trim_end().to_string())
    }
}

/// The output as text: a string, the text items of `content`, or JSON.
fn output_payload(call: &ToolCall) -> Payload {
    match &call.output {
        Some(value) => Payload::from_value(value),
        None => match &call.content {
            Some(Value::Array(items)) => {
                let text = items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                Payload::from_text(&text)
            }
            Some(other) => Payload::from_value(other),
            None => Payload::plain(String::new()),
        },
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

/// Label + payload body (plain mono or pretty JSON), with a line cap.
/// Soft pane around the content only — the tool header stays flush.
fn tool_section(
    ctx: &PartCtx<'_>,
    what: &str,
    label: Option<&str>,
    payload: &Payload,
    truncated: bool,
    line_cap: usize,
) -> Div {
    let strings = &ctx.app.strings;
    let all = ctx.is_open(&format!("{what}.all"), false);
    let (shown, more) = if all {
        (payload.text.trim_end().to_string(), 0)
    } else {
        clip_lines(&payload.text, line_cap)
    };
    let body = if payload.json {
        div().class("mq-tool-plain").w_full().child(
            rich_text_styled(crate::ui::markdown::highlight::highlight(
                Some("json"),
                &shown,
            ))
            .size(scale::FS_12)
            .line_height(1.5)
            .font_family(FontFamily::named(MONO_FONT)),
        )
    } else {
        div()
            .class("mq-tool-plain")
            .w_full()
            .child(mono_block(shown.clone()))
    };
    let mut section = div().class("mq-tool-section").w_full().flex_col();
    if let Some(label) = label.filter(|s| !s.is_empty()) {
        let mut head = div()
            .class("mq-tool-section-head")
            .w_full()
            .flex_row()
            .items_center()
            .child(
                meta(label.to_string())
                    .class("mq-tool-section-label")
                    .no_cursor(),
            );
        if payload.json {
            head = head.child(div().flex_1()).child(markdown::copy_button(
                &ctx.key(&format!("{what}.copy")),
                strings.copy(),
                shown.clone(),
            ));
        }
        section = section.child(head);
    }
    let mut pane = div().class("mq-tool-pane").w_full().flex_col().child(body);
    if more > 0 || all {
        let expand = if all {
            strings.show_less().to_string()
        } else {
            format!("{} · {}", strings.more_lines(more), strings.show_all())
        };
        pane = pane.child(link_button(expand, ctx.toggle(&format!("{what}.all"))));
    }
    if truncated {
        pane = pane.child(meta("…"));
    }
    section.child(pane)
}

/// Tools that stay quiet once done (no pane chrome while collapsed).
fn is_quiet_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "read"
            | "readfile"
            | "read_file"
            | "search"
            | "grep"
            | "glob"
            | "list"
            | "ls"
            | "find"
            | "fetch"
            | "webfetch"
            | "web_fetch"
            | "get"
            | "cat"
            | "view"
            | "other"
    )
}

fn tool_label(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "read" | "readfile" | "read_file" | "cat" | "view" => "Read".into(),
        "search" | "grep" => "Search".into(),
        "glob" | "find" => "Glob".into(),
        "list" | "ls" => "List".into(),
        "fetch" | "webfetch" | "web_fetch" | "get" => "Fetch".into(),
        "bash" | "shell" | "run" | "terminal" => "Run".into(),
        "edit" | "write" | "apply_patch" | "applypatch" => "Edit".into(),
        "other" => "Other".into(),
        _ if !name.is_empty() => {
            let mut chars = name.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                None => name.to_string(),
            }
        }
        _ => name.to_string(),
    }
}

/// Drop the session cwd prefix so args read as relative paths.
fn relativise(path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return path.to_string();
    };
    let cwd = cwd.trim_end_matches(['/', '\\']);
    path.strip_prefix(cwd)
        .map(|rest| rest.trim_start_matches(['/', '\\']))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// The call's input on one line: the field that says most (the command, the
/// path, the pattern) or else the first string.
fn args_summary(call: &ToolCall, cwd: Option<&str>) -> String {
    const PATH_KEYS: &[&str] = &["filePath", "file_path", "path"];
    const BODY_KEYS: &[&str] = &[
        "command",
        "cmd",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
        "content",
        "new_string",
        "newString",
        "old_string",
        "oldString",
    ];
    let line = |v: &str| v.lines().next().unwrap_or("").trim().to_string();
    if let Value::Object(map) = &call.input {
        let path = PATH_KEYS
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str))
            .map(|p| relativise(&line(p), cwd));
        let body = BODY_KEYS
            .iter()
            .find_map(|k| map.get(*k).and_then(Value::as_str))
            .map(line)
            .filter(|v| !v.is_empty());
        if let Some(path) = path {
            let shown = match body.filter(|v| v != &path) {
                Some(extra) => format!("{path} · {}", extra.chars().take(120).collect::<String>()),
                None => path,
            };
            return shown.chars().take(200).collect();
        }
        if let Some(v) = body.or_else(|| map.values().find_map(Value::as_str).map(line)) {
            let shown = relativise(&v, cwd);
            return shown.chars().take(200).collect();
        }
    }
    match (&call.title, &call.input_partial) {
        (Some(title), _) if title != &call.name => relativise(&line(title), cwd),
        (_, Some(partial)) => relativise(&line(partial), cwd).chars().take(200).collect(),
        _ => String::new(),
    }
}

/// A tool call, one 36 px row: `▸ name  args … duration`, a 3 px bar on the
/// left for the state (info running with a spinner at the right, grey done,
/// red failed, amber while an approval for it waits). Opened, it shows the
/// input and the output. Completed passive tools render as quiet rows.
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
    let has_media = call.content.as_ref().is_some_and(|c| {
        c.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                let mime = item.get("mime").and_then(|v| v.as_str());
                let uri = item
                    .get("uri")
                    .or_else(|| item.get("url"))
                    .or_else(|| item.get("path"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                matches!(
                    item.get("type").and_then(|v| v.as_str()),
                    Some("image" | "video" | "file")
                ) || matches!(
                    media::kind_for(uri, mime),
                    media::MediaKind::Image | media::MediaKind::Video
                )
            })
        })
    });
    // Shape only — never parse/pretty here (runs every stream rebuild).
    let has_structured = match &call.output {
        Some(Value::Object(_) | Value::Array(_)) => true,
        Some(Value::String(s)) => looks_like_json_object(s),
        _ => match &call.input {
            Value::Array(_) => true,
            Value::Object(map) => map
                .values()
                .any(|v| matches!(v, Value::Object(_) | Value::Array(_))),
            _ => false,
        },
    };
    let quiet = !waiting
        && !unfinished
        && status == ToolCallStatus::Completed
        && !stale
        && !has_media
        && !has_structured
        && is_quiet_tool(&call.name);
    // Media always opens; structured JSON waits until the call settles so
    // syntect is not paid on every streaming delta.
    let open = ctx.is_open(
        "tool",
        has_media || (has_structured && !unfinished && !waiting),
    );
    let cwd = ctx
        .app
        .bindings
        .header
        .get()
        .and_then(|h| h.directory.clone());
    let args = args_summary(call, cwd.as_deref());
    let label = tool_label(&call.name);
    let name_tone = if waiting {
        "mq-tone--warning"
    } else if status == ToolCallStatus::Failed {
        "mq-tone--failed"
    } else if unfinished && !stale {
        "mq-tone--running"
    } else {
        ""
    };
    let mut name = div().class("mq-tool-name").flex_shrink_0();
    if !name_tone.is_empty() {
        name = name.class(name_tone);
    }
    name = name.child(if quiet {
        meta(label).no_cursor()
    } else {
        mono(label).no_cursor()
    });
    let mut head = div()
        .id(ctx.key("tool"))
        .class("mq-tool-row")
        .w_full()
        .h(if quiet { 28.0 } else { ROW_HEIGHT - 2.0 })
        .flex_row()
        .items_center()
        .min_w(0.0)
        .cursor_pointer()
        .child(crate::ui::icons::chevron(open))
        .child(name)
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
        let input = Payload::from_input(&call.input, call.input_partial.as_deref());
        if !input.text.trim().is_empty() {
            body = body.child(tool_section(
                ctx,
                "in",
                Some(strings.input()),
                &input,
                false,
                INPUT_LINES,
            ));
        }
        let output = output_payload(call);
        if !output.text.trim().is_empty() {
            body = body.child(tool_section(
                ctx,
                "out",
                Some(strings.output()),
                &output,
                call.truncated,
                OUTPUT_LINES,
            ));
        }
        if let Some(content) = call.content.as_ref() {
            if let Some(media_row) = media::tool_content_media(ctx, content) {
                body = body.child(
                    div()
                        .class("mq-tool-section")
                        .w_full()
                        .child(
                            div()
                                .class("mq-tool-pane")
                                .class("mq-tool-pane--media")
                                .w_full()
                                .child(media_row),
                        ),
                );
            }
        }
    }
    if let Some(err) = &call.error {
        body = body.child(error_line(err));
    }
    // Quiet collapsed stays a single line. Everything else is a flush header
    // with soft panes on expanded content — no whole-row accent frame.
    if quiet && !open {
        div()
            .class("mq-tool-quiet")
            .w_full()
            .flex_row()
            .items_stretch()
            .child(body.flex_1().min_w(0.0))
    } else {
        body.class("mq-tool").w_full()
    }
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
            body = body.child(tool_section(
                ctx,
                "cmd",
                None,
                &Payload::plain(command.to_string()),
                false,
                INPUT_LINES,
            ));
        }
        if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
            body = body.child(tool_section(
                ctx,
                "out",
                None,
                &Payload::from_text(out),
                truncated,
                OUTPUT_LINES,
            ));
        }
    }
    card(accent, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_inputs_stay_readable_lines() {
        let input = serde_json::json!({"command": "ls -la", "timeout": 30});
        assert_eq!(
            Payload::from_input(&input, None),
            Payload::plain("command: ls -la\ntimeout: 30")
        );
        assert_eq!(
            Payload::from_input(&Value::Null, Some("{\"pa")),
            Payload::plain("{\"pa")
        );
    }

    #[test]
    fn nested_or_json_string_outputs_are_pretty() {
        let nested = serde_json::json!({"ok": true, "items": [1, 2]});
        let pretty = Payload::from_value(&nested);
        assert!(pretty.json);
        assert!(pretty.text.contains("\n"));
        assert!(pretty.text.contains("\"items\""));

        let as_string = Payload::from_text(r#"{"a":1,"b":[2,3]}"#);
        assert!(as_string.json);
        assert!(as_string.text.contains("\n  "));

        assert!(!Payload::from_text("not json {").json);
        assert!(!Payload::from_text("test result: ok").json);
    }

    #[test]
    fn multiline_input_values_open_as_json() {
        let input = serde_json::json!({"command": "ls -la\nwc -l", "timeout": 30});
        let payload = Payload::from_input(&input, None);
        assert!(payload.json);
        assert!(payload.text.contains("ls -la\\nwc -l") || payload.text.contains("ls -la"));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(420), "420ms");
        assert_eq!(duration(4200), "4.2s");
        assert_eq!(duration(125_000), "2m 5s");
    }

    #[test]
    fn args_prefer_path_then_body() {
        let call = ToolCall {
            id: "c".into(),
            name: "edit".into(),
            title: None,
            input: serde_json::json!({
                "path": "/tmp/opencode/desktop-qa/src/a.rs",
                "new_string": "fn main() {}"
            }),
            output: None,
            content: None,
            metadata: None,
            state: ToolCallStatus::Completed,
            status: ToolCallStatus::Completed,
            error: None,
            child_session_id: None,
            background: false,
            input_partial: None,
            truncated: false,
            time: Default::default(),
        };
        assert_eq!(
            args_summary(&call, Some("/tmp/opencode/desktop-qa")),
            "src/a.rs · fn main() {}"
        );
    }
}
