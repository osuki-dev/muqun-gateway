//! Claude Code's session transcripts: `~/.claude/projects/<slug>/<uuid>.jsonl`.
//!
//! One JSON object per line. The main conversation is the `user` and
//! `assistant` lines that are not `isSidechain`; `system` lines with text and
//! `summary` lines are shown as system rows; every other type (snapshots,
//! queue operations, request blobs, ...) is bookkeeping and is skipped.
//! An assistant reply is written one content block per line, all lines
//! carrying the same `message.id`, so those lines group into one message.
//! A `tool_use` is paired with the `tool_result` block (in a later `user`
//! line) that names its id. Thinking text is never served, only a marker;
//! images are never served, only an attachment placeholder.

use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;

use super::{
    fence, line_message_id, AnswerLookup, LineClass, LocateHints, Projection, SourceLine,
    TranscriptReader,
};
use crate::agents::domain::{
    part_item_id, reasoning_item_id, text_item_id, tool_item_id, AgentErrorInfo, AgentPart,
    CompactionStatus, TimelineItem, TimelineRole, ToolCall, ToolCallStatus, ToolTime,
};

const AGENT: &str = "claude";
/// Tool output kept per call. Longer output is cut and marked `truncated`.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
/// Any one string inside a tool's input (a whole file passed to `Write`).
const MAX_TOOL_INPUT_STRING_BYTES: usize = 16 * 1024;
const MAX_CWD_BYTES: usize = 4096;
/// Room for each line's blocks in the message's ordinal space.
const ORDINALS_PER_LINE: u64 = 1024;

pub(crate) struct ClaudeReader;

impl TranscriptReader for ClaudeReader {
    fn agent(&self) -> &'static str {
        AGENT
    }

    fn root(&self) -> Option<PathBuf> {
        let config = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))?;
        Some(config.join("projects"))
    }

    fn valid_session_id(&self, id: &str) -> bool {
        uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.hyphenated().to_string() == id)
    }

    fn locate(&self, root: &Path, session_id: &str, hints: &LocateHints) -> Option<PathBuf> {
        let file_name = format!("{session_id}.jsonl");
        if let Some(cwd) = hints
            .cwd
            .as_deref()
            .filter(|cwd| cwd.len() <= MAX_CWD_BYTES)
        {
            let slug = project_slug(cwd);
            if !slug.is_empty() {
                if let Some(found) = fence::admit(root, &root.join(slug).join(&file_name)) {
                    return Some(found);
                }
            }
        }
        // The cwd is unknown, moved, or too long for Claude's plain slug.
        fence::find_in_children(root, &file_name)
    }

    fn classify(&self, line: &[u8]) -> Option<LineClass> {
        let probe: Probe = serde_json::from_slice(line).ok()?;
        if probe.sidechain == Some(true) {
            return Some(LineClass::default());
        }
        let content = probe.message.as_ref().and_then(|m| m.content.as_ref());
        Some(match probe.kind.as_deref() {
            Some("user") => match content {
                Some(ProbeContent::Text(nonempty)) => LineClass {
                    visible: *nonempty,
                    ..LineClass::default()
                },
                Some(ProbeContent::Blocks(blocks)) => LineClass {
                    visible: blocks
                        .iter()
                        .any(|block| block.kind.as_deref() != Some("tool_result")),
                    group: None,
                    answers: blocks
                        .iter()
                        .filter(|block| block.kind.as_deref() == Some("tool_result"))
                        .filter_map(|block| block.tool_use_id.clone())
                        .collect(),
                },
                None => LineClass::default(),
            },
            Some("assistant") => LineClass {
                visible: true,
                group: probe.message.and_then(|message| message.id),
                answers: Vec::new(),
            },
            Some("system") => LineClass {
                visible: probe.content.is_some(),
                ..LineClass::default()
            },
            Some("summary") => LineClass {
                visible: probe.summary.is_some(),
                ..LineClass::default()
            },
            _ => LineClass::default(),
        })
    }

    fn project(&self, lines: &[SourceLine], lookup: &mut dyn AnswerLookup) -> Projection {
        let mut projection = Projection::default();
        for line in lines {
            match serde_json::from_slice::<Value>(&line.bytes) {
                // A line with only unknown block types yields nothing; that
                // is not an error.
                Ok(row) if row.is_object() => {
                    project_line(line, &row, lookup, &mut projection.items)
                }
                _ => projection.malformed += 1,
            }
        }
        projection
    }
}

/// Claude Code's project directory name: every character that is not an
/// ASCII letter or digit becomes `-` (per UTF-16 unit, as JavaScript sees it).
pub(crate) fn project_slug(cwd: &str) -> String {
    let mut slug = String::with_capacity(cwd.len());
    for character in cwd.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else {
            for _ in 0..character.len_utf16() {
                slug.push('-');
            }
        }
    }
    slug
}

// --- indexing probe ------------------------------------------------------

#[derive(Deserialize)]
struct Probe {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    message: Option<ProbeMessage>,
    content: Option<IgnoredAny>,
    summary: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct ProbeMessage {
    id: Option<String>,
    content: Option<ProbeContent>,
}

#[derive(Deserialize)]
struct ProbeBlock {
    #[serde(rename = "type")]
    kind: Option<String>,
    tool_use_id: Option<String>,
}

/// `message.content`: a string (whether it has text) or content blocks.
/// Hand-written so a block's large fields are skipped, never buffered.
enum ProbeContent {
    Text(bool),
    Blocks(Vec<ProbeBlock>),
}

impl<'de> Deserialize<'de> for ProbeContent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = ProbeContent;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or an array of content blocks")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(ProbeContent::Text(!text.trim().is_empty()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element::<Option<ProbeBlock>>()? {
                    blocks.extend(block);
                }
                Ok(ProbeContent::Blocks(blocks))
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

// --- projection ----------------------------------------------------------

struct Row<'a> {
    message_id: String,
    role: TimelineRole,
    line: u64,
    base: u64,
    updated_ms: u64,
    items: &'a mut Vec<TimelineItem>,
}

impl Row<'_> {
    fn push(&mut self, block: u64, part: AgentPart, attachments: Option<Vec<String>>) {
        let ordinal = self.base + block;
        let id = match &part {
            AgentPart::Text { .. } => text_item_id(&self.message_id, ordinal),
            AgentPart::Reasoning { .. } => reasoning_item_id(&self.message_id, ordinal),
            AgentPart::Tool(call) => tool_item_id(&self.message_id, &call.id),
            _ => part_item_id(&self.message_id, ordinal),
        };
        self.items.push(TimelineItem {
            id,
            message_id: self.message_id.clone(),
            role: self.role,
            part,
            seq: self.line + 1,
            updated_ms: self.updated_ms,
            ordinal,
            attachments,
            image_assets: None,
        });
    }
}

fn project_line(
    line: &SourceLine,
    row: &Value,
    lookup: &mut dyn AnswerLookup,
    items: &mut Vec<TimelineItem>,
) {
    let kind = row["type"].as_str().unwrap_or_default();
    let role = match kind {
        "user" => TimelineRole::User,
        "assistant" => TimelineRole::Assistant,
        "system" | "summary" => TimelineRole::System,
        _ => return,
    };
    let mut out = Row {
        message_id: line_message_id(AGENT, line.group_start),
        role,
        line: line.line,
        base: line.line.saturating_sub(line.group_start) * ORDINALS_PER_LINE,
        updated_ms: timestamp_ms(&row["timestamp"]).unwrap_or(0),
        items,
    };
    match kind {
        "system" => {
            if let Some(text) = row["content"].as_str().filter(|t| !t.trim().is_empty()) {
                out.push(
                    0,
                    AgentPart::System {
                        text: text.into(),
                        description: row["subtype"].as_str().map(str::to_owned),
                    },
                    None,
                );
            }
        }
        "summary" => {
            if let Some(text) = row["summary"].as_str() {
                out.push(
                    0,
                    AgentPart::System {
                        text: text.into(),
                        description: Some("summary".into()),
                    },
                    None,
                );
            }
        }
        "user" => project_user(&mut out, row),
        _ => project_assistant(&mut out, row, lookup),
    }
}

fn user_text_part(row: &Value, text: String) -> AgentPart {
    if row["isCompactSummary"] == true {
        AgentPart::Compaction {
            status: CompactionStatus::Completed,
            reason: None,
            summary: Some(text),
            recent: None,
            keep_message_id: None,
            tokens: None,
            cost: None,
            error: None,
        }
    } else if row["isMeta"] == true {
        AgentPart::Synthetic {
            text,
            description: None,
        }
    } else {
        AgentPart::Text { text }
    }
}

fn project_user(out: &mut Row, row: &Value) {
    let content = &row["message"]["content"];
    if let Some(text) = content.as_str() {
        out.push(0, user_text_part(row, text.into()), None);
        return;
    }
    let Some(blocks) = content.as_array() else {
        return;
    };
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for (at, block) in blocks.iter().enumerate() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    texts.push((at as u64, text.to_owned()));
                }
            }
            Some("image") => images.push(format!("[Image #{}]", images.len() + 1)),
            _ => {}
        }
    }
    let attachments = (!images.is_empty()).then_some(images);
    if texts.is_empty() {
        if attachments.is_some() {
            out.push(
                0,
                AgentPart::Text {
                    text: String::new(),
                },
                attachments,
            );
        }
        return;
    }
    let mut attachments = attachments;
    for (at, text) in texts {
        out.push(at, user_text_part(row, text), attachments.take());
    }
}

fn project_assistant(out: &mut Row, row: &Value, lookup: &mut dyn AnswerLookup) {
    let Some(blocks) = row["message"]["content"].as_array() else {
        if let Some(text) = row["message"]["content"].as_str() {
            out.push(0, AgentPart::Text { text: text.into() }, None);
        }
        return;
    };
    for (at, block) in blocks.iter().enumerate() {
        let at = at as u64;
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    out.push(at, AgentPart::Text { text: text.into() }, None);
                }
            }
            // The marker only: thinking text is never served.
            Some("thinking" | "redacted_thinking") => out.push(
                at,
                AgentPart::Reasoning {
                    text: String::new(),
                    duration_ms: row["thinkingDurationMs"].as_u64(),
                },
                None,
            ),
            Some("tool_use") => {
                if let Some(call) = tool_call(block, out.updated_ms, lookup) {
                    out.push(at, AgentPart::Tool(call), None);
                }
            }
            _ => {}
        }
    }
}

fn tool_call(block: &Value, ran_ms: u64, lookup: &mut dyn AnswerLookup) -> Option<ToolCall> {
    let id = block["id"].as_str().filter(|id| !id.is_empty())?;
    let name = block["name"].as_str().unwrap_or("tool").to_owned();
    let mut input = block["input"].clone();
    let mut truncated = cap_strings(&mut input, MAX_TOOL_INPUT_STRING_BYTES);
    let result = lookup.answer(id).and_then(|answer| {
        let row: Value = serde_json::from_slice(&answer.bytes).ok()?;
        let block = row["message"]["content"]
            .as_array()?
            .iter()
            .find(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)?
            .clone();
        Some((block, timestamp_ms(&row["timestamp"])))
    });
    // No result yet is "running": the transcript has not recorded one, which
    // is all it can say. Nothing here infers completion from idleness.
    let (state, output, error, completed) = match result {
        None => (ToolCallStatus::Running, None, None, None),
        Some((block, completed)) => {
            let (text, cut) = result_text(&block["content"]);
            truncated |= cut;
            let failed = block["is_error"] == true;
            let error = failed.then(|| AgentErrorInfo {
                name: "ClaudeToolError".into(),
                message: cap(&text, 1024).0,
                status: None,
            });
            let state = if failed {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            (state, Some(Value::String(text)), error, completed)
        }
    };
    Some(ToolCall {
        id: id.into(),
        title: tool_title(&input),
        name,
        input,
        output,
        content: None,
        metadata: None,
        state,
        status: state,
        error,
        child_session_id: None,
        background: false,
        input_partial: None,
        truncated,
        time: ToolTime {
            created: None,
            ran: (ran_ms > 0).then_some(ran_ms),
            completed,
        },
    })
}

/// A tool result's text: a string, or text blocks joined, with images as
/// placeholders. Capped.
fn result_text(content: &Value) -> (String, bool) {
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().map(str::to_owned),
                Some("image") => Some("[Image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    cap(&text, MAX_TOOL_OUTPUT_BYTES)
}

fn tool_title(input: &Value) -> Option<String> {
    [
        "description",
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "prompt",
    ]
    .iter()
    .find_map(|key| {
        input[*key]
            .as_str()
            .filter(|value| !value.trim().is_empty())
    })
    .map(|value| {
        let line = value.lines().next().unwrap_or_default();
        cap(line, 160).0
    })
}

fn cap(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_owned(), true)
}

/// Cut every string in `value` longer than `max`; whether any was cut.
fn cap_strings(value: &mut Value, max: usize) -> bool {
    match value {
        Value::String(text) if text.len() > max => {
            *text = cap(text, max).0;
            true
        }
        Value::Array(values) => values
            .iter_mut()
            .fold(false, |cut, value| cap_strings(value, max) | cut),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |cut, value| cap_strings(value, max) | cut),
        _ => false,
    }
}

/// `2026-10-22T08:15:30.123Z` (or with an offset) as Unix milliseconds.
fn timestamp_ms(value: &Value) -> Option<u64> {
    let text = value.as_str()?;
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> { text.get(range)?.parse().ok() };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        let padded = format!("{:0<3}", &fraction[..digits.min(3)]);
        millis = padded.parse().ok()?;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 60
                + rest.get(4..6)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(seconds * 1000 + millis).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// Synthetic Claude Code lines, shaped like the real format.
    pub(crate) fn user(uuid: &str, content: Value) -> String {
        json!({
            "type": "user", "uuid": uuid, "parentUuid": null, "isSidechain": false,
            "timestamp": "2026-10-22T08:00:00.000Z", "sessionId": "s", "cwd": "/w",
            "message": { "role": "user", "content": content }, "futureField": { "x": 1 }
        })
        .to_string()
    }

    pub(crate) fn assistant(uuid: &str, message_id: &str, blocks: Value) -> String {
        json!({
            "type": "assistant", "uuid": uuid, "isSidechain": false,
            "timestamp": "2026-10-22T08:00:01.500Z", "thinkingDurationMs": 1200,
            "message": { "id": message_id, "role": "assistant", "content": blocks }
        })
        .to_string()
    }

    pub(crate) fn tool_result(
        uuid: &str,
        tool_use_id: &str,
        content: Value,
        error: bool,
    ) -> String {
        json!({
            "type": "user", "uuid": uuid, "isSidechain": false,
            "timestamp": "2026-10-22T08:00:03.000Z",
            "message": { "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": error }
            ] },
            "toolUseResult": { "stdout": "ignored" }
        })
        .to_string()
    }

    #[test]
    fn session_ids_must_be_canonical_uuids() {
        let reader = ClaudeReader;
        assert!(reader.valid_session_id("c0ffee00-2810-41ce-8abd-bc97683d3c6e"));
        for bad in [
            "",
            "../etc/passwd",
            "C0FFEE00-2810-41CE-8ABD-BC97683D3C6E",
            "c0ffee0028104 1ce8abdbc97683d3c6e",
            "c0ffee00281041ce8abdbc97683d3c6e",
            "c0ffee00-2810-41ce-8abd-bc97683d3c6e.jsonl",
        ] {
            assert!(!reader.valid_session_id(bad), "{bad}");
        }
    }

    #[test]
    fn the_project_slug_replaces_every_non_alphanumeric_character() {
        assert_eq!(
            project_slug("/home/ryu/Work/muqun/app"),
            "-home-ryu-Work-muqun-app"
        );
        assert_eq!(project_slug("/tmp/a_b.c d"), "-tmp-a-b-c-d");
        // One dash per UTF-16 unit, as Claude Code's JavaScript counts.
        assert_eq!(project_slug("/x/文件"), "-x---");
        assert_eq!(project_slug("/x/😀"), "-x---");
    }

    #[test]
    fn classification_hides_bookkeeping_sidechains_and_bare_tool_results() {
        let reader = ClaudeReader;
        let class = |line: &str| reader.classify(line.as_bytes());
        assert!(class(&user("u1", json!("hello"))).unwrap().visible);
        assert!(!class(&user("u1", json!("  "))).unwrap().visible);
        let result = class(&tool_result("u2", "toolu_1", json!("ok"), false)).unwrap();
        assert!(!result.visible);
        assert_eq!(result.answers, ["toolu_1"]);
        let reply = class(&assistant(
            "a1",
            "msg_1",
            json!([{"type":"text","text":"x"}]),
        ))
        .unwrap();
        assert!(reply.visible);
        assert_eq!(reply.group.as_deref(), Some("msg_1"));
        let mut side: Value = serde_json::from_str(&user("u3", json!("hi"))).unwrap();
        side["isSidechain"] = json!(true);
        assert_eq!(class(&side.to_string()).unwrap(), LineClass::default());
        for hidden in [
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
            r#"{"type":"api-request-blob","message":{"content":[{"type":"text","text":"x"}]}}"#,
            r#"{"type":"system","subtype":"turn_duration","durationMs":5}"#,
        ] {
            assert!(!class(hidden).unwrap().visible, "{hidden}");
        }
        assert!(
            class(r#"{"type":"system","subtype":"informational","content":"x"}"#)
                .unwrap()
                .visible
        );
        assert!(
            class(r#"{"type":"summary","summary":"Title","leafUuid":"u"}"#)
                .unwrap()
                .visible
        );
        assert_eq!(class("{not json"), None);
        assert_eq!(class("[1,2]"), None);
    }

    struct Answers(Vec<(String, String)>);

    impl AnswerLookup for Answers {
        fn answer(&mut self, key: &str) -> Option<SourceLine> {
            self.0
                .iter()
                .find(|(id, _)| id == key)
                .map(|(_, line)| SourceLine {
                    line: 99,
                    group_start: 99,
                    bytes: line.clone().into_bytes(),
                })
        }
    }

    fn source(line: u64, group_start: u64, text: &str) -> SourceLine {
        SourceLine {
            line,
            group_start,
            bytes: text.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_page_projects_text_thinking_tools_and_images_into_timeline_items() {
        let lines = [
            source(
                0,
                0,
                &user(
                    "u1",
                    json!([
                        { "type": "text", "text": "Look at this" },
                        { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }
                    ]),
                ),
            ),
            source(
                1,
                1,
                &assistant(
                    "a1",
                    "msg_1",
                    json!([{ "type": "thinking", "thinking": "secret plan", "signature": "sig" }]),
                ),
            ),
            source(
                2,
                1,
                &assistant(
                    "a2",
                    "msg_1",
                    json!([{ "type": "text", "text": "Reading it." }]),
                ),
            ),
            source(
                3,
                1,
                &assistant(
                    "a3",
                    "msg_1",
                    json!([
                        { "type": "tool_use", "id": "toolu_ok", "name": "Bash", "input": { "command": "ls -la\nmore", "description": "List files" } },
                        { "type": "tool_use", "id": "toolu_bad", "name": "Read", "input": { "file_path": "/w/x" } },
                        { "type": "tool_use", "id": "toolu_open", "name": "Grep", "input": { "pattern": "x" } },
                        { "type": "future_block", "x": 1 }
                    ]),
                ),
            ),
            source(5, 5, "{\"type\":\"user\",\"message\":"),
        ];
        let mut answers = Answers(vec![
            (
                "toolu_ok".into(),
                tool_result(
                    "r1",
                    "toolu_ok",
                    json!([{ "type": "text", "text": "a\nb" }, { "type": "image", "source": {} }]),
                    false,
                ),
            ),
            (
                "toolu_bad".into(),
                tool_result("r2", "toolu_bad", json!("no such file"), true),
            ),
        ]);
        let projection = ClaudeReader.project(&lines, &mut answers);
        assert_eq!(projection.malformed, 1);
        let items = projection.items;
        assert_eq!(items.len(), 6);

        // The user's text carries the image as a placeholder attachment.
        assert_eq!(items[0].role, TimelineRole::User);
        assert_eq!(
            items[0].part,
            AgentPart::Text {
                text: "Look at this".into()
            }
        );
        assert_eq!(items[0].attachments, Some(vec!["[Image #1]".to_owned()]));
        assert_eq!(items[0].message_id, "tx_claude_000000000000");

        // Thinking is a marker without its text.
        assert_eq!(
            items[1].part,
            AgentPart::Reasoning {
                text: String::new(),
                duration_ms: Some(1200)
            }
        );
        assert!(!serde_json::to_string(&items)
            .unwrap()
            .contains("secret plan"));
        assert!(!serde_json::to_string(&items).unwrap().contains("AAAA"));

        // One assistant message across three lines, in order.
        let ids: Vec<&str> = items[1..].iter().map(|i| i.message_id.as_str()).collect();
        assert_eq!(ids, ["tx_claude_000000000001"; 5]);
        let ordinals: Vec<u64> = items[1..].iter().map(|i| i.ordinal).collect();
        assert!(
            ordinals.windows(2).all(|pair| pair[0] < pair[1]),
            "{ordinals:?}"
        );

        let tool = |item: &TimelineItem| match &item.part {
            AgentPart::Tool(call) => call.clone(),
            other => panic!("{other:?}"),
        };
        let ok = tool(&items[3]);
        assert_eq!(items[3].id, "tx_claude_000000000001:tool:toolu_ok");
        assert_eq!(ok.state, ToolCallStatus::Completed);
        assert_eq!(ok.output, Some(json!("a\nb\n[Image]")));
        assert_eq!(ok.title.as_deref(), Some("List files"));
        assert_eq!(ok.time.completed, Some(1_792_656_003_000));
        let bad = tool(&items[4]);
        assert_eq!(bad.state, ToolCallStatus::Failed);
        assert_eq!(bad.error.unwrap().message, "no such file");
        let open = tool(&items[5]);
        assert_eq!(open.state, ToolCallStatus::Running);
        assert_eq!(open.output, None);
    }

    #[test]
    fn compact_summaries_meta_system_and_summary_lines_have_their_own_parts() {
        let mut compact: Value =
            serde_json::from_str(&user("u", json!("The story so far"))).unwrap();
        compact["isCompactSummary"] = json!(true);
        let mut meta: Value = serde_json::from_str(&user("m", json!("injected"))).unwrap();
        meta["isMeta"] = json!(true);
        let lines = [
            source(0, 0, &compact.to_string()),
            source(1, 1, &meta.to_string()),
            source(
                2,
                2,
                r#"{"type":"system","subtype":"informational","content":"Note","timestamp":"2026-10-22T08:00:00+02:00"}"#,
            ),
            source(
                3,
                3,
                r#"{"type":"summary","summary":"Session title","leafUuid":"x"}"#,
            ),
        ];
        let items = ClaudeReader.project(&lines, &mut Answers(Vec::new())).items;
        assert!(
            matches!(&items[0].part, AgentPart::Compaction { summary: Some(s), .. } if s == "The story so far")
        );
        assert!(matches!(&items[1].part, AgentPart::Synthetic { text, .. } if text == "injected"));
        assert_eq!(
            items[2].part,
            AgentPart::System {
                text: "Note".into(),
                description: Some("informational".into())
            }
        );
        assert_eq!(items[2].updated_ms, 1_792_648_800_000);
        assert_eq!(items[3].role, TimelineRole::System);
    }

    #[test]
    fn long_tool_input_and_output_are_capped() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 10);
        let lines = [source(
            0,
            0,
            &assistant(
                "a",
                "m",
                json!([
                    { "type": "tool_use", "id": "t", "name": "Write", "input": { "content": big } }
                ]),
            ),
        )];
        let mut answers = Answers(vec![("t".into(), tool_result("r", "t", json!(big), false))]);
        let items = ClaudeReader.project(&lines, &mut answers).items;
        let AgentPart::Tool(call) = &items[0].part else {
            panic!()
        };
        assert!(call.truncated);
        assert_eq!(
            call.input["content"].as_str().unwrap().len(),
            MAX_TOOL_INPUT_STRING_BYTES
        );
        assert_eq!(
            call.output.as_ref().unwrap().as_str().unwrap().len(),
            MAX_TOOL_OUTPUT_BYTES
        );
    }
}
