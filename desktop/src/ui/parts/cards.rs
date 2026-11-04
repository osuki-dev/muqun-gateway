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
use super::tool_kind::{self, ToolKind};
use super::{card, clip_lines, link_button, media, meta, mono, mono_block, scale, PartCtx};
use crate::model::commands::Command;
use crate::ui::glyphs;
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
        parse_tool_output(value, 0)
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

/// How far App's `parseToolOutput` unwraps before showing what it has.
const TOOL_OUTPUT_MAX_DEPTH: usize = 6;

/// Unwrap OpenCode / MCP / Cursor result shapes into display text (App parity).
fn parse_tool_output(raw: &Value, depth: usize) -> Payload {
    if depth >= TOOL_OUTPUT_MAX_DEPTH {
        return pretty_structured(raw)
            .map(Payload::json)
            .unwrap_or_else(|| Payload::plain(raw.to_string()));
    }
    match raw {
        Value::Null => Payload::plain(String::new()),
        Value::String(s) => {
            let trimmed = s.trim();
            if looks_like_json_object(trimmed) {
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    return parse_tool_output(&value, depth + 1);
                }
            }
            Payload::plain(s.trim_end().to_string())
        }
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                let text = match item {
                    Value::String(s) => s.as_str(),
                    other => other.get("text").and_then(Value::as_str).unwrap_or(""),
                };
                if text.is_empty() {
                    continue;
                }
                // Shell exit line is shown as a chip, not body text.
                if text_contains_exit_line(text) {
                    continue;
                }
                parts.push(text);
            }
            Payload::plain(parts.join("\n"))
        }
        Value::Object(rec) => {
            if let Some(Value::String(stdout)) = rec.get("stdout") {
                return Payload::plain(stdout.trim_end().to_string());
            }
            if let Some(output) = rec.get("output") {
                return parse_tool_output(output, depth + 1);
            }
            // App unwraps array `content`; Cursor ACP often sends `{content: "…"}`.
            if let Some(content) = rec.get("content") {
                match content {
                    Value::Array(_) | Value::String(_) | Value::Object(_) => {
                        return parse_tool_output(content, depth + 1);
                    }
                    _ => {}
                }
            }
            if let Some(text) = rec.get("text") {
                return parse_tool_output(text, depth + 1);
            }
            pretty_structured(raw)
                .map(Payload::json)
                .unwrap_or_else(|| Payload::plain(raw.to_string()))
        }
        other => Payload::plain(other.to_string()),
    }
}

fn text_contains_exit_line(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("command exited with code")
}

/// Content text items first (App); else unwrap `output` / `rawOutput`.
fn output_payload(call: &ToolCall) -> Payload {
    if let Some(Value::Array(items)) = &call.content {
        let text = items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .filter(|t| !t.is_empty() && !text_contains_exit_line(t))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            return Payload::from_text(&text);
        }
    }
    match &call.output {
        Some(value) => parse_tool_output(value, 0),
        None => match &call.content {
            Some(other) => parse_tool_output(other, 0),
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
    let lang = if payload.json { Some("json") } else { None };
    code_section(ctx, what, label, &payload.text, lang, truncated, line_cap)
}

/// Code / plain body with optional section label and line cap (App CodeBody /
/// OutputLines — no mandatory Input/Output chrome).
fn code_section(
    ctx: &PartCtx<'_>,
    what: &str,
    label: Option<&str>,
    text: &str,
    lang: Option<&str>,
    truncated: bool,
    line_cap: usize,
) -> Div {
    let strings = &ctx.app.strings;
    let all = ctx.is_open(&format!("{what}.all"), false);
    let (shown, more) = if all {
        (text.trim_end().to_string(), 0)
    } else {
        clip_lines(text, line_cap)
    };
    let body = if let Some(lang) = lang {
        div().class("mq-tool-plain").w_full().child(
            rich_text_styled(crate::ui::markdown::highlight::highlight(
                Some(lang),
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
        if lang == Some("json") {
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

/// Drop the session cwd prefix so paths read as relative.
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

/// Display name for the tool chip: App shows the wire name; image is special.
fn tool_name_label(kind: ToolKind, name: &str) -> String {
    match kind {
        ToolKind::Image => "Image".into(),
        _ if !name.is_empty() => name.to_string(),
        _ => "tool".into(),
    }
}

/// Checklist items from a todo tool's input (App `readTodoItems`).
fn todo_items_from_input(input: &Value) -> Vec<TodoItem> {
    let Some(arr) = input
        .get("todos")
        .or_else(|| input.get("items"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            if let Some(text) = entry.as_str() {
                return Some(TodoItem {
                    text: text.to_owned(),
                    done: false,
                });
            }
            let text = entry
                .get("content")
                .or_else(|| entry.get("text"))
                .or_else(|| entry.get("title"))
                .and_then(Value::as_str)?
                .to_owned();
            let done = entry.get("status").and_then(Value::as_str) == Some("completed")
                || entry.get("done").and_then(Value::as_bool).unwrap_or(false);
            Some(TodoItem { text, done })
        })
        .collect()
}

/// A tool call: App-shaped header (kind icon, wire name, target) and a body
/// specialised per family when opened — edit/patch show metadata diffs,
/// everything else shows input/output panes.
pub fn tool(ctx: &PartCtx<'_>, call: &ToolCall) -> Div {
    let strings = &ctx.app.strings;
    let status = call.state;
    let wire_name = if call.name.is_empty() {
        call.input
            .get("_toolName")
            .and_then(Value::as_str)
            .unwrap_or("")
    } else {
        call.name.as_str()
    };
    let kind = tool_kind::classify(wire_name);
    // Todo tools render as the checklist plate, never a generic tool card.
    if kind == ToolKind::Todo {
        let items = todo_items_from_input(&call.input);
        if !items.is_empty() {
            return todo(ctx, &items);
        }
    }
    let waiting = ctx
        .turn
        .as_ref()
        .is_some_and(|t| t.waiting_on(&ctx.item.id));
    let unfinished = matches!(
        status,
        ToolCallStatus::Pending | ToolCallStatus::Streaming | ToolCallStatus::Running
    );
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
    let edit_files = tool_kind::edit_files_from_metadata(call.metadata.as_ref());
    let shell_exit = tool_kind::shell_exit(call.metadata.as_ref());
    let shell_bad = kind == ToolKind::Shell
        && (tool_kind::shell_timed_out(call.metadata.as_ref())
            || shell_exit.is_some_and(|c| c != 0));
    let cwd = ctx
        .app
        .bindings
        .header
        .get()
        .and_then(|h| h.directory.clone());
    let raw_target = tool_kind::extract_target(kind, &call.input, call.input_partial.as_deref());
    let target = relativise(&raw_target, cwd.as_deref());
    // `read` of a picture: show the image (App ToolFiles preview), don't quiet-collapse.
    let path_is_media = matches!(
        media::kind_for(&raw_target, None),
        media::MediaKind::Image | media::MediaKind::Video
    );
    let open = ctx.is_open("tool", path_is_media && kind == ToolKind::Read);
    let skill = tool_kind::skill_name(call.metadata.as_ref());
    let (mut title, mut caption) = if kind == ToolKind::Skill {
        (
            skill.clone().unwrap_or_else(|| target.clone()),
            String::new(),
        )
    } else {
        tool_kind::header_lines(kind, &target, call.title.as_deref())
    };
    if title.is_empty() {
        title = call
            .title
            .clone()
            .filter(|t| t != &call.name)
            .unwrap_or_default();
    }
    // App headers are `numberOfLines={1}`; hard newlines in titles used to
    // paint a squashed two-line row inside the fixed-height tool head.
    let scrub = |s: &str| match kind {
        ToolKind::Shell | ToolKind::Execute => tool_kind::command_line(s),
        _ => tool_kind::one_line(s),
    };
    title = scrub(&title);
    caption = scrub(&caption);
    let label = tool_name_label(kind, wire_name);
    // Never echo the chip name twice ("SendMessage SendMessage", "Agent Agent").
    if title.eq_ignore_ascii_case(&label) || title.eq_ignore_ascii_case(wire_name) {
        title.clear();
    }
    // Routine shells/reads, or any finished call with nothing left to say
    // beyond the chip name, collapse to a quiet one-liner (App parity).
    // Image/video reads keep a real body — App puts them in the preview slot.
    let quiet = !waiting
        && !unfinished
        && status == ToolCallStatus::Completed
        && !stale
        && !has_media
        && !(kind == ToolKind::Read && path_is_media)
        && !shell_bad
        && edit_files.is_empty()
        && (kind.is_routine() || (title.is_empty() && caption.is_empty()));
    let name_tone = if waiting {
        "mq-tone--warning"
    } else if status == ToolCallStatus::Failed || shell_bad {
        "mq-tone--failed"
    } else if unfinished && !stale {
        "mq-tone--running"
    } else {
        ""
    };
    let tint = ThemeState::get().color(ColorToken::TextSecondary);
    let mut name = div()
        .class("mq-tool-name")
        .flex_row()
        .items_center()
        .flex_shrink_0();
    if !name_tone.is_empty() {
        name = name.class(name_tone);
    }
    name = name
        .child(glyphs::icon(kind.glyph(), 13.0).tint(tint))
        .child(if quiet {
            meta(label.clone()).no_cursor()
        } else {
            mono(label.clone()).no_cursor()
        });
    let mut title_col = div()
        .class("mq-tool-title")
        .flex_1()
        .min_w(0.0)
        .overflow_clip()
        .flex_col();
    if !title.is_empty() && !title.eq_ignore_ascii_case(&label) {
        title_col = title_col.child(mono(title.clone()).size(scale::FS_12).no_cursor());
    }
    if !caption.is_empty() && caption != title && !caption.eq_ignore_ascii_case(&label) {
        title_col = title_col.child(
            meta(relativise(&caption, cwd.as_deref()))
                .class("mq-text-subtle")
                .no_cursor(),
        );
    }
    let mut head = div()
        .id(ctx.key("tool"))
        .class("mq-tool-row")
        .w_full()
        .h(if quiet {
            28.0
        } else if caption.is_empty() {
            ROW_HEIGHT - 2.0
        } else {
            44.0
        })
        .flex_row()
        .items_center()
        .min_w(0.0)
        .cursor_pointer()
        .child(crate::ui::icons::chevron(open))
        .child(name)
        .child(title_col);
    if let Some(n) = tool_kind::result_count(call.metadata.as_ref()) {
        head = head.child(meta(format!("{n}")).class("mq-text-subtle").no_cursor());
    }
    if let Some(code) = shell_exit {
        let chip = if code == 0 {
            meta(format!("exit {code}")).class("mq-text-subtle")
        } else {
            meta(format!("exit {code}")).class("mq-tone--failed")
        };
        head = head.child(chip.no_cursor());
    }
    if !edit_files.is_empty() {
        let add: u32 = edit_files.iter().map(|f| f.additions).sum();
        let del: u32 = edit_files.iter().map(|f| f.deletions).sum();
        head = head.child(
            meta(format!("+{add} −{del}"))
                .class("mq-text-subtle")
                .no_cursor(),
        );
    }
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
        if matches!(kind, ToolKind::Edit | ToolKind::Patch | ToolKind::Write)
            && !edit_files.is_empty()
        {
            for (i, file) in edit_files.iter().enumerate() {
                let (shown, more) = clip_lines(&file.patch, DIFF_PREVIEW_LINES as usize);
                let mut pane = div()
                    .class("mq-tool-pane")
                    .w_full()
                    .flex_col()
                    .child(
                        meta(format!(
                            "{}  +{} −{}",
                            relativise(&file.path, cwd.as_deref()),
                            file.additions,
                            file.deletions
                        ))
                        .no_cursor(),
                    )
                    .child(
                        div().class("mq-tool-plain").w_full().child(
                            rich_text_styled(diff_text(&shown))
                                .size(scale::FS_12)
                                .line_height(1.5)
                                .font_family(FontFamily::named(MONO_FONT)),
                        ),
                    );
                if more > 0 {
                    pane = pane.child(meta(format!("+{more}…")));
                }
                body = body.child(
                    div()
                        .class("mq-tool-section")
                        .w_full()
                        .flex_col()
                        .child(pane),
                );
                let _ = i;
            }
        } else {
            body = append_tool_body(body, ctx, call, kind, &target, &raw_target, call.truncated);
        }
        let mut showed_media = false;
        if let Some(content) = call.content.as_ref() {
            if let Some(media_row) = media::tool_content_media(ctx, content) {
                showed_media = true;
                body = body.child(
                    div().class("mq-tool-section").w_full().child(
                        div()
                            .class("mq-tool-pane")
                            .class("mq-tool-pane--media")
                            .w_full()
                            .child(media_row),
                    ),
                );
            }
        }
        // `read` of an image often only names the path — no content[] media entry.
        if !showed_media && kind == ToolKind::Read {
            if let Some(media_row) = media::path_media(ctx, &raw_target) {
                body = body.child(
                    div().class("mq-tool-section").w_full().child(
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

/// Per-family body — App `renderToolBody`: specialised panes, not raw JSON walls.
fn append_tool_body(
    mut body: Div,
    ctx: &PartCtx<'_>,
    call: &ToolCall,
    kind: ToolKind,
    target: &str,
    raw_target: &str,
    truncated: bool,
) -> Div {
    let output = output_payload(call);
    match kind {
        ToolKind::Shell => {
            if !target.is_empty() {
                body = body.child(
                    div().class("mq-tool-section").w_full().child(
                        div()
                            .class("mq-tool-pane")
                            .w_full()
                            .child(mono_block(target.to_string())),
                    ),
                );
            }
            if !output.text.trim().is_empty() {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &output.text,
                    None,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
        ToolKind::Read => {
            // Pictures/videos: the media pane (added by the caller from
            // `raw_target`) is the body — don't dump path args or binary text.
            if matches!(
                media::kind_for(raw_target, None),
                media::MediaKind::Image | media::MediaKind::Video
            ) {
                return body;
            }
            let text = tool_kind::prepare_read_text(&output.text, target);
            if text.is_empty() {
                return body;
            }
            // Directory listings stay plain; file bodies get a fence language.
            let lang = if text.starts_with("Read directory ") || target.ends_with('/') {
                None
            } else {
                tool_kind::language_for_path(target)
            };
            body = body.child(code_section(
                ctx,
                "out",
                None,
                &text,
                lang,
                truncated,
                OUTPUT_LINES,
            ));
        }
        ToolKind::Write => {
            if let Some(contents) = tool_kind::write_content(&call.input) {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &contents,
                    tool_kind::language_for_path(target),
                    truncated,
                    OUTPUT_LINES,
                ));
            } else if !output.text.trim().is_empty() {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &output.text,
                    None,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
        ToolKind::Edit => {
            // No metadata.files: assemble a before/after patch from the args.
            if let Some(patch) = synthetic_edit_patch(target, &call.input) {
                let (shown, more) = clip_lines(&patch, DIFF_PREVIEW_LINES as usize);
                let mut pane = div().class("mq-tool-pane").w_full().flex_col().child(
                    div().class("mq-tool-plain").w_full().child(
                        rich_text_styled(diff_text(&shown))
                            .size(scale::FS_12)
                            .line_height(1.5)
                            .font_family(FontFamily::named(MONO_FONT)),
                    ),
                );
                if more > 0 {
                    pane = pane.child(meta(format!("+{more}…")));
                }
                body = body.child(
                    div()
                        .class("mq-tool-section")
                        .w_full()
                        .flex_col()
                        .child(pane),
                );
            } else if !output.text.trim().is_empty() {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &output.text,
                    None,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
        ToolKind::Glob => {
            let groups = tool_kind::group_paths_by_directory(&output.text);
            if groups.is_empty() {
                if !output.text.trim().is_empty() {
                    body = body.child(code_section(
                        ctx,
                        "out",
                        None,
                        &output.text,
                        None,
                        truncated,
                        OUTPUT_LINES,
                    ));
                }
            } else {
                body = body.child(glob_groups_pane(&groups));
            }
        }
        ToolKind::Grep => {
            let groups = tool_kind::group_grep_matches(&output.text);
            if groups.is_empty() {
                if !output.text.trim().is_empty() {
                    body = body.child(code_section(
                        ctx,
                        "out",
                        None,
                        &output.text,
                        None,
                        truncated,
                        OUTPUT_LINES,
                    ));
                }
            } else {
                body = body.child(grep_groups_pane(&groups));
            }
        }
        ToolKind::Web => {
            if !output.text.trim().is_empty() {
                let setting = ctx.setting(false);
                let key = ctx.key("web");
                let chrome = ctx.chrome(&key);
                body = body.child(div().class("mq-tool-section").w_full().child(
                    div().class("mq-tool-pane").w_full().child(markdown::render(
                        &markdown::parse(&output.text),
                        &setting,
                        &chrome,
                    )),
                ));
            }
        }
        ToolKind::Skill => {
            let desc = tool_kind::tool_input_record(&call.input, None)
                .and_then(|r| {
                    r.get("description")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| output.text.clone());
            if !desc.trim().is_empty() {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &desc,
                    None,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
        ToolKind::Execute => {
            if let Some(code) = tool_kind::tool_input_record(&call.input, None)
                .and_then(|r| r.get("code").and_then(Value::as_str).map(str::to_string))
            {
                body = body.child(code_section(
                    ctx,
                    "code",
                    None,
                    &code,
                    Some("javascript"),
                    false,
                    INPUT_LINES,
                ));
            }
            if !output.text.trim().is_empty() {
                body = body.child(code_section(
                    ctx,
                    "out",
                    None,
                    &output.text,
                    None,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
        _ => {
            // Generic / MCP: arguments as highlighted JSON when nested; result
            // as unwrapped text — never a labelled JSON dump of `{content:…}`.
            let strings = &ctx.app.strings;
            let input = Payload::from_input(&call.input, call.input_partial.as_deref());
            let show_labels = !input.text.trim().is_empty() && !output.text.trim().is_empty();
            if !input.text.trim().is_empty() {
                let lang = if input.json { Some("json") } else { None };
                body = body.child(code_section(
                    ctx,
                    "in",
                    show_labels.then_some(strings.input()),
                    &input.text,
                    lang,
                    false,
                    INPUT_LINES,
                ));
            }
            if !output.text.trim().is_empty() {
                let lang = if output.json { Some("json") } else { None };
                body = body.child(code_section(
                    ctx,
                    "out",
                    show_labels.then_some(strings.output()),
                    &output.text,
                    lang,
                    truncated,
                    OUTPUT_LINES,
                ));
            }
        }
    }
    body
}

fn glob_groups_pane(groups: &[tool_kind::PathGroup]) -> Div {
    let mut pane = div().class("mq-tool-pane").w_full().flex_col();
    for group in groups.iter().take(8) {
        pane = pane.child(
            meta(if group.directory.is_empty() {
                "/".into()
            } else {
                group.directory.clone()
            })
            .class("mq-text-subtle")
            .no_cursor(),
        );
        for name in group.files.iter().take(12) {
            pane = pane.child(mono(name.clone()).no_cursor());
        }
        if group.files.len() > 12 {
            pane = pane.child(
                meta(format!("… {}", group.files.len() - 12))
                    .class("mq-text-subtle")
                    .no_cursor(),
            );
        }
    }
    if groups.len() > 8 {
        pane = pane.child(
            meta(format!("… {} dirs", groups.len() - 8))
                .class("mq-text-subtle")
                .no_cursor(),
        );
    }
    div()
        .class("mq-tool-section")
        .w_full()
        .flex_col()
        .child(pane)
}

fn grep_groups_pane(groups: &[tool_kind::GrepFileMatches]) -> Div {
    let mut pane = div().class("mq-tool-pane").w_full().flex_col();
    for group in groups.iter().take(12) {
        pane = pane.child(mono(group.file.clone()).no_cursor());
        for hit in group.lines.iter().take(8) {
            let line = match hit.line {
                Some(n) => format!("{n}: {}", hit.text),
                None => hit.text.clone(),
            };
            pane = pane.child(meta(line).class("mq-text-subtle").no_cursor());
        }
        if group.lines.len() > 8 {
            pane = pane.child(
                meta(format!("… {}", group.lines.len() - 8))
                    .class("mq-text-subtle")
                    .no_cursor(),
            );
        }
    }
    if groups.len() > 12 {
        pane = pane.child(
            meta(format!("… {} files", groups.len() - 12))
                .class("mq-text-subtle")
                .no_cursor(),
        );
    }
    div()
        .class("mq-tool-section")
        .w_full()
        .flex_col()
        .child(pane)
}

fn synthetic_edit_patch(path: &str, input: &Value) -> Option<String> {
    let rec = tool_kind::tool_input_record(input, None)?;
    let old = rec
        .get("oldString")
        .or_else(|| rec.get("old_string"))
        .and_then(Value::as_str);
    let new = rec
        .get("newString")
        .or_else(|| rec.get("new_string"))
        .and_then(Value::as_str);
    if old.is_none() && new.is_none() {
        return None;
    }
    let mut lines = vec![format!("@@ {path} @@")];
    if let Some(old) = old {
        for line in old.split('\n') {
            lines.push(format!("-{line}"));
        }
    }
    if let Some(new) = new {
        for line in new.split('\n') {
            lines.push(format!("+{line}"));
        }
    }
    Some(lines.join("\n"))
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

/// A to-do list as a collapsible checklist (App `AgentTodoBlock`).
pub fn todo(ctx: &PartCtx<'_>, items: &[TodoItem]) -> Div {
    if items.is_empty() {
        return div();
    }
    let done = items.iter().filter(|i| i.done).count();
    let total = items.len();
    let all_done = done == total;
    let open = ctx.is_open("todo", false);
    let first_pending = items.iter().position(|i| !i.done);
    let theme = ThemeState::get();
    let tint = if all_done {
        theme.color(ColorToken::Success)
    } else {
        theme.color(ColorToken::Primary)
    };
    let head = div()
        .id(ctx.key("todo"))
        .class("mq-todo-head")
        .w_full()
        .flex_row()
        .items_center()
        .cursor_pointer()
        .child(glyphs::icon(glyphs::Glyph::TaskList, 14.0).tint(tint))
        .child(
            div()
                .flex_1()
                .min_w(0.0)
                .child(meta(ctx.app.strings.todo_title())),
        )
        .child(meta(format!("{done}/{total}")).class(if all_done {
            "mq-todo-count mq-todo-count--done"
        } else {
            "mq-todo-count"
        }))
        .child(crate::ui::icons::chevron(open))
        .on_click(ctx.toggle("todo"));

    let track_w = (ctx.width - 8.0).max(48.0);
    let fill_w = if total == 0 {
        0.0
    } else {
        track_w * (done as f32 / total as f32)
    };
    let progress = div()
        .class("mq-todo-progress")
        .class(if all_done {
            "mq-todo-progress--done"
        } else {
            ""
        })
        .w(track_w)
        .h(2.0)
        .child(div().w(fill_w).h(2.0));

    let mut list = div()
        .class("mq-todo-card")
        .w_full()
        .flex_col()
        .child(head)
        .child(progress);
    if open {
        let mut body = div().class("mq-todo-body").w_full().flex_col();
        for (idx, item) in items.iter().enumerate() {
            let current = !item.done && Some(idx) == first_pending;
            let mark = if item.done {
                glyphs::icon(glyphs::Glyph::Done, 14.0).tint(theme.color(ColorToken::Success))
            } else if current {
                glyphs::icon(glyphs::Glyph::Time, 14.0).tint(tint)
            } else {
                glyphs::icon(glyphs::Glyph::Pending, 13.0)
                    .tint(theme.color(ColorToken::TextTertiary))
            };
            let text_class = if item.done {
                "mq-todo mq-todo--done"
            } else if current {
                "mq-todo mq-todo--current"
            } else {
                "mq-todo"
            };
            let label = if item.done {
                // App strikes completed items; approximate with muted text.
                ui_text(item.text.clone())
                    .size(scale::FS_13)
                    .line_height(1.5)
                    .class("mq-text-subtle")
            } else {
                ui_text(item.text.clone())
                    .size(scale::FS_13)
                    .line_height(1.5)
                    .class("mq-text")
            };
            body = body.child(
                div()
                    .class(text_class)
                    .w_full()
                    .flex_row()
                    .items_start()
                    .child(div().class("mq-glyph").flex_shrink_0().child(mark))
                    .child(div().flex_1().min_w(0.0).child(label)),
            );
        }
        list = list.child(body);
    }
    list
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
                .child(mono(tool_kind::command_line(command)).no_cursor()),
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
    fn cursor_content_envelope_unwraps_to_file_text() {
        let raw = serde_json::json!({
            "content": ".mq-tool { color: red; }\n"
        });
        let payload = Payload::from_value(&raw);
        assert!(!payload.json);
        assert!(payload.text.contains(".mq-tool"));
        assert!(!payload.text.contains("\"content\""));
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
    fn edit_header_keeps_the_path_on_one_line() {
        let input = serde_json::json!({
            "path": "/tmp/opencode/desktop-qa/src/a.rs",
            "new_string": "fn main() {}"
        });
        let target = tool_kind::extract_target(ToolKind::Edit, &input, None);
        let (title, caption) = tool_kind::header_lines(ToolKind::Edit, &target, None);
        assert_eq!(title, "/tmp/opencode/desktop-qa/src/a.rs");
        assert!(caption.is_empty());
    }
}
