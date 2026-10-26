//! The task plumbing a session transcript is served without.
//!
//! A delegated agent says how it ended with `tasks.complete` or, without
//! the tool, a marker line ([`pane_agent::DONE_MARKER`],
//! [`pane_agent::BLOCKED_MARKER`]); sessions from before goals went alone
//! also hold the completion instruction this gateway used to send.
//! Both are how the gateway learns the task's end; the reader only needs the
//! goal and the answer. So on the way out (snapshot, timeline, event replay,
//! the SSE stream and `/api/ws`, like `message_images`) the transcript -- and
//! the inbox -- of a session a task is bound to ([`for_session`]) is served
//! with:
//!
//! - a user text (or a synthetic message) that ends with a completion
//!   instruction -- any wording this gateway has sent, found by its fixed
//!   opening and its closing placeholder line, not by the goal -- cut back to
//!   what came before it, and dropped when nothing did;
//! - each turn's last assistant message without the one marker the ledger
//!   read the turn's end from ([`pane_agent::consumed_marker`] over its
//!   text rows joined, as `service::turn_end` reads them): cut from where it
//!   starts -- a line's start, or after the agent's words on it -- to the
//!   line's end, the words before it kept, unless the reader sees that line
//!   inside a code fence; every other marker stays in view, and a row left
//!   with nothing is dropped. A streamed text
//!   is served as snapshots of the whole text so far, so while the session
//!   runs a turn a last line that could only be the start of a marker (bare
//!   markup such as `- ` included) is held back too: the marker never
//!   flashes before its turn ends;
//! - OpenCode's "Code Mode tool catalog has changed" system notice when
//!   it is exactly that. Tool calls, including catalog discovery, stay visible.
//!
//! An event carries only the rows it updates, so the live stream reads its
//! message as the turn's last: a marker in an earlier message of the turn
//! is cut live but stays in the snapshot, which sees the whole turn.
//!
//! A streamed row left with nothing is removed (`agent.timeline.removed`)
//! rather than left out, so a client never keeps an older version of it.
//!
//! Skill-loading rows keep their existing presentation policy. Muqun tool
//! calls remain visible in every lifecycle state, both in task sessions and
//! ordinary conversations. The mirror, the agent's own reads and the task
//! follower keep the raw text: detection (`pane_agent::marker`,
//! `service::turn_end`) reads what the agent wrote.

use super::pane_agent::{
    self, BLOCKED_MARKER, DONE_MARKER, REASON_PLACEHOLDER, REQUEST_OPENING, SUMMARY_PLACEHOLDER,
};
use crate::agents::domain::{AgentDomainEvent, AgentPart, TimelineItem, TimelineRole};
use crate::AppState;

/// How session `asid`'s transcript is served: a task session omits internal
/// instructions and completion markers, holding back a last line still being written
/// when it is `streaming` (running a turn).
pub(crate) async fn for_session(state: &AppState, asid: &str) -> Plumbing {
    Plumbing {
        streaming: Streaming(state.tasks.is_busy(asid)),
        task: state.tasks.is_task_session(asid).await,
    }
}

/// What a session is served without (see [`for_session`]).
#[derive(Clone, Copy)]
pub(crate) struct Plumbing {
    pub(crate) streaming: Streaming,
    /// A task is (or was) bound to the session: markers, the completion
    /// instruction and catalog notices are sanitized.
    pub(crate) task: bool,
}

/// A task session's plumbing, as the tests mostly read it.
impl From<Streaming> for Plumbing {
    fn from(streaming: Streaming) -> Self {
        Self {
            streaming,
            task: true,
        }
    }
}

/// Whether the session is running a turn: its rows' last lines may still
/// be being written. A finished message is never cut but for its markers.
#[derive(Clone, Copy)]
pub(crate) struct Streaming(pub(crate) bool);

/// `items` as the reader sees them.
pub(crate) fn hide_in_items(items: &mut Vec<TimelineItem>, plumbing: impl Into<Plumbing>) {
    let plumbing = plumbing.into();
    if plumbing.task {
        strip_consumed_markers(items, plumbing.streaming);
    }
    items.retain_mut(|item| hide_in_item(item, plumbing) != Served::Emptied);
}

/// Cut from `items` the marker line the ledger read each turn's end from:
/// [`pane_agent::consumed_marker`] over the text of the turn's last
/// assistant message, its text rows joined as `service::turn_end` joins
/// them -- unless, as the reader sees that row, it sits in a code fence.
/// While `streaming`, each such message's last row also loses a last line
/// that may still become a marker. Every other marker line stays in view.
fn strip_consumed_markers(items: &mut [TimelineItem], streaming: Streaming) {
    let text_of = |item: &TimelineItem| match (&item.role, &item.part) {
        (TimelineRole::Assistant, AgentPart::Text { text }) => Some(text.clone()),
        _ => None,
    };
    // The last assistant message with text before each user row, and at the end.
    let mut last_messages: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for item in items.iter() {
        if item.role == TimelineRole::User {
            last_messages.extend(current.take());
        } else if text_of(item).is_some_and(|text| !text.trim().is_empty()) {
            current = Some(item.message_id.clone());
        }
    }
    last_messages.extend(current);
    for message in last_messages {
        let rows: Vec<usize> = (0..items.len())
            .filter(|&at| items[at].message_id == message && text_of(&items[at]).is_some())
            .collect();
        let texts: Vec<String> = rows.iter().filter_map(|&at| text_of(&items[at])).collect();
        let consumed = pane_agent::consumed_marker(&texts.join("\n"));
        let mut first_line = 0;
        for (n, (&at, text)) in rows.iter().zip(&texts).enumerate() {
            let count = text.split('\n').count();
            let here = consumed
                .filter(|(line, _)| (first_line..first_line + count).contains(line))
                .map(|(line, start)| (line - first_line, start));
            let last_row = n + 1 == rows.len();
            if let Some(served) = without_marker_line(text, here, streaming.0 && last_row) {
                if let AgentPart::Text { text } = &mut items[at].part {
                    *text = served;
                }
            }
            first_line += count;
        }
    }
}

/// `text` without the marker the ledger consumed from it, if one (outside
/// a code fence, as the reader sees it): from where it starts to its line's
/// end, the agent's words before it kept -- and, while `streaming`, without
/// a last line that may still become a marker.
fn without_marker_line(
    text: &str,
    consumed: Option<(usize, usize)>,
    streaming: bool,
) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let fenced = |at: usize| {
        lines[..at]
            .iter()
            .filter(|line| pane_agent::is_fence(line))
            .count()
            % 2
            == 1
    };
    let consumed = consumed.filter(|&(at, _)| at <= last && !fenced(at));
    // While streaming, the last line is held from where a marker may be
    // starting: the whole line, or what follows the agent's words on it.
    let held = if streaming && !fenced(last) {
        held_from(lines[last])
    } else {
        None
    };
    if consumed.is_none() && held.is_none() {
        return None;
    }
    // The marker runs from where it starts to the line's end; the agent's
    // words before it on that line stay, bare bullets or markup do not.
    let cut = |line: &'_ str, start: usize| -> Option<String> {
        let before = line.get(..start).unwrap_or("").trim_end();
        before
            .chars()
            .any(char::is_alphanumeric)
            .then(|| before.to_owned())
    };
    let kept: Vec<String> = lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| match (consumed, held) {
            (Some((line_at, start)), _) if line_at == at => cut(line, start),
            (_, Some(start)) if at == last => cut(line, start),
            _ => Some((*line).to_owned()),
        })
        .collect();
    Some(kept.join("\n").trim_end().to_owned())
}

#[derive(PartialEq)]
enum Served {
    Kept,
    Emptied,
}

fn hide_in_item(item: &mut TimelineItem, plumbing: Plumbing) -> Served {
    // Keep the existing policy for skill-loading setup rows. Tool execution
    // and discovery remain visible, including Muqun's own MCP tools.
    if let AgentPart::Tool(call) = &item.part {
        if call.name.eq_ignore_ascii_case("skill") {
            return Served::Emptied;
        }
    }
    if !plumbing.task {
        return Served::Kept;
    }
    let (text, served) = match (item.role, &mut item.part) {
        (_, AgentPart::System { text, .. }) | (_, AgentPart::Synthetic { text, .. })
            if is_catalog_notice(text) =>
        {
            return Served::Emptied;
        }
        // The notice the task's per-session instruction leaves.
        (_, AgentPart::System { text, description })
        | (_, AgentPart::Synthetic { text, description })
            if is_task_instruction_notice(text, description.as_deref()) =>
        {
            return Served::Emptied;
        }
        (TimelineRole::User, AgentPart::Text { text }) | (_, AgentPart::Synthetic { text, .. }) => {
            let served = without_request(text);
            (text, served)
        }
        // Its marker line was cut before ([`strip_consumed_markers`]).
        (TimelineRole::Assistant, AgentPart::Text { text }) if text.trim().is_empty() => {
            return Served::Emptied;
        }
        _ => return Served::Kept,
    };
    if let Some(served) = served {
        *text = served;
        if text.trim().is_empty() {
            return Served::Emptied;
        }
    }
    Served::Kept
}

/// `event` as the reader sees it. An upsert whose every row is left with
/// nothing becomes the removal of those rows.
pub(crate) fn hide_in_event(event: &mut AgentDomainEvent, plumbing: impl Into<Plumbing>) {
    let plumbing = plumbing.into();
    match event {
        AgentDomainEvent::TimelineUpsert { asid, items, seq } => {
            if plumbing.task {
                strip_consumed_markers(items, plumbing.streaming);
            }
            let mut emptied = Vec::new();
            items.retain_mut(|item| {
                let kept = hide_in_item(item, plumbing) == Served::Kept;
                if !kept {
                    emptied.push(item.id.clone());
                }
                kept
            });
            if items.is_empty() && !emptied.is_empty() {
                *event = AgentDomainEvent::TimelineRemoved {
                    asid: asid.clone(),
                    ids: emptied,
                    seq: *seq,
                };
            }
        }
        AgentDomainEvent::InboxChanged { items, .. } if plumbing.task => hide_in_inbox(items),
        _ => {}
    }
}

/// A session's inbox (OpenCode's `{type, payload: {text}, ...}` items, the
/// input admitted but not yet run) as the reader sees it: the instruction
/// waits there beside the goal for a moment.
pub(crate) fn hide_in_inbox(items: &mut Vec<serde_json::Value>) {
    items.retain_mut(|item| {
        let Some(text) = item.pointer_mut("/payload/text") else {
            return true;
        };
        let Some(served) = text.as_str().and_then(without_request) else {
            return true;
        };
        let keep = !served.trim().is_empty();
        *text = served.into();
        keep
    });
}

/// The first line of OpenCode's notice that its Code Mode tool catalog
/// changed (the muqun tools joining it); the catalog listing follows it
/// under a `# Code Mode` heading.
const CATALOG_NOTICE: [&str; 2] = [
    "The Code Mode tool catalog has changed.",
    "The Code Mode tool catalog has changed. This catalog supersedes the previous Code Mode tool catalog.",
];

/// Whether a system or synthetic row is the notice of the task's
/// per-session instruction (`mcp::inject::SESSION_INSTRUCTION_KEY`): its
/// description names the key, or its text is the instruction.
fn is_task_instruction_notice(text: &str, description: Option<&str>) -> bool {
    let key = crate::mcp::inject::SESSION_INSTRUCTION_KEY;
    description.is_some_and(|description| {
        description.starts_with("Instructions updated") && description.contains(key)
    }) || text.trim().starts_with(crate::mcp::inject::TASK_SENTENCE)
}

/// Whether `text` is exactly that notice: its first line, then nothing or
/// the catalog listing.
fn is_catalog_notice(text: &str) -> bool {
    let text = text.trim();
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let rest = rest.trim_start();
    CATALOG_NOTICE.contains(&first.trim()) && (rest.is_empty() || rest.starts_with("# Code Mode\n"))
}

/// `text` without the completion instruction it ends with, if it ends with
/// one.
fn without_request(text: &str) -> Option<String> {
    let trimmed = text.trim_end();
    let closes = [
        format!("{DONE_MARKER} {SUMMARY_PLACEHOLDER}"),
        format!("{BLOCKED_MARKER} {REASON_PLACEHOLDER}"),
    ];
    if !closes.iter().any(|close| trimmed.ends_with(close.as_str())) {
        return None;
    }
    let at = trimmed.rfind(REQUEST_OPENING)?;
    let before = &trimmed[..at];
    (before.is_empty() || before.ends_with(char::is_whitespace))
        .then(|| before.trim_end().to_owned())
}

/// `text` for a line the ledger shows of its own (an idle task's hint, a
/// question, `progress`): every marker, valid or not, cut from where it
/// starts to the end of its line, with the quote or backtick before it;
/// a line left with no words is dropped. A hint never carries marker text.
pub(crate) fn without_any_marker(text: &str) -> String {
    if !text.contains(DONE_MARKER) && !text.contains(BLOCKED_MARKER) {
        return text.to_owned();
    }
    text.lines()
        .filter_map(|line| {
            let start = [DONE_MARKER, BLOCKED_MARKER]
                .iter()
                .filter_map(|marker| line.find(marker))
                .min();
            let Some(start) = start else {
                return Some(line);
            };
            let before = line[..start].trim_end_matches(['`', '"', '\'', '“', '‘', '*', ' ']);
            before.chars().any(char::is_alphanumeric).then_some(before)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where `line`, the last line still being written, may be starting a
/// marker: 0 when the whole line may become one, else the byte after the
/// agent's words where a marker's start follows them ("help?MUQUN_TASK_DO").
fn held_from(line: &str) -> Option<usize> {
    if may_become_marker(line) {
        return Some(0);
    }
    let start = line
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_' || *c == ':'))
        .map_or(0, |(at, c)| at + c.len_utf8());
    let tail = &line[start..];
    let partial = !tail.is_empty()
        && [DONE_MARKER, BLOCKED_MARKER]
            .iter()
            .any(|marker| marker.starts_with(tail) || tail.starts_with(marker));
    (partial && start > 0).then_some(start)
}

/// Whether `line`, the last line still being written, may become a marker
/// line: a marker or its start, or bare list or emphasis markup (not a
/// `---` rule).
fn may_become_marker(line: &str) -> bool {
    let body = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    if !body.is_empty() {
        return [DONE_MARKER, BLOCKED_MARKER]
            .iter()
            .any(|marker| body.starts_with(marker) || marker.starts_with(body));
    }
    let markup = line.trim();
    let mut chars = markup.chars();
    let rule = markup.chars().count() >= 3 && chars.next().is_some_and(|c| chars.all(|d| d == c));
    !markup.is_empty()
        && markup.chars().count() <= 4
        && markup.chars().all(|c| "-*+•⏺`>_ ".contains(c))
        && !rule
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::pane_agent::LAST_REQUEST;

    fn item(role: TimelineRole, part: AgentPart) -> TimelineItem {
        TimelineItem {
            id: "i".into(),
            message_id: "m".into(),
            role,
            part,
            seq: 1,
            updated_ms: 0,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
        }
    }

    fn text(text: &str) -> AgentPart {
        AgentPart::Text { text: text.into() }
    }

    /// An OpenCode tool call `name`, `status`, with `code` as its input.
    fn tool(name: &str, status: &str, code: serde_json::Value) -> AgentPart {
        AgentPart::Tool(crate::agents::adapters::opencode::mapper::map_tool_call(
            &serde_json::json!({ "type": "tool", "id": "c", "name": name,
                "state": { "status": status, "input": { "code": code } } }),
        ))
    }

    #[test]
    fn the_reader_gets_the_goal_and_the_answer_without_the_plumbing() {
        use TimelineRole::{Assistant, System, User};
        let current = format!("fix the build\n\n{LAST_REQUEST}");
        // Every wording this gateway has sent.
        let first = "fix the build\n\nWhen the whole task is finished, print one line exactly \
                     in this form: MUQUN_TASK_DONE: <one-line summary>";
        let second = "fix the build\n\nWhen the whole task is finished, call the muqun tool \
                      tasks.complete with a one-line summary; if you cannot call it, print one \
                      line exactly in this form: MUQUN_TASK_DONE: <one-line summary>";
        let instruction_only = current.trim_start_matches("fix the build\n\n");
        let synthetic = AgentPart::Synthetic {
            text: instruction_only.into(),
            description: None,
        };
        for (name, role, part, served) in [
            (
                "current request",
                User,
                text(&current),
                Some("fix the build"),
            ),
            ("first wording", User, text(first), Some("fix the build")),
            ("second wording", User, text(second), Some("fix the build")),
            ("instruction alone", User, text(instruction_only), None),
            ("synthetic instruction", System, synthetic, None),
            (
                "a user quoting it mid-text",
                User,
                text("say When the whole task is finished, ok"),
                Some("say When the whole task is finished, ok"),
            ),
            (
                "done line",
                Assistant,
                text("Hi there.\n\nMUQUN_TASK_DONE: Responded to your greeting."),
                Some("Hi there."),
            ),
            (
                "bulleted blocked line",
                Assistant,
                text("Stopped.\n- **MUQUN_TASK_BLOCKED:** notes.md is missing\n"),
                Some("Stopped."),
            ),
            (
                "marker only",
                Assistant,
                text("MUQUN_TASK_DONE: done"),
                None,
            ),
            (
                "a marker after the agent's words on its line",
                Assistant,
                text("Hello. How can I help?MUQUN_TASK_DONE: Greeted the user"),
                Some("Hello. How can I help?"),
            ),
            (
                "a marker mid-text",
                Assistant,
                text("one\nMUQUN_TASK_DONE: x\ntwo"),
                Some("one\ntwo"),
            ),
            (
                "plain answer",
                Assistant,
                text("MUQUN is a word\nok"),
                Some("MUQUN is a word\nok"),
            ),
            (
                "user marker is the user's",
                User,
                text("MUQUN_TASK_DONE: x"),
                Some("MUQUN_TASK_DONE: x"),
            ),
            (
                "a marker quoted in a code fence",
                Assistant,
                text("The line:\n```\nMUQUN_TASK_DONE: x\n```"),
                Some("The line:\n```\nMUQUN_TASK_DONE: x\n```"),
            ),
            (
                "a closing rule",
                Assistant,
                text("All green.\n---"),
                Some("All green.\n---"),
            ),
            (
                "the catalog notice",
                System,
                AgentPart::System {
                    text: "The Code Mode tool catalog has changed. This catalog supersedes the previous \
                           Code Mode tool catalog.\n\n# Code Mode\n\nUse the `execute` tool."
                        .into(),
                    description: None,
                },
                None,
            ),
            (
                "the task's per-session instruction notice",
                System,
                AgentPart::System {
                    text: crate::mcp::inject::TASK_SENTENCE.into(),
                    description: Some("Instructions updated: muqun-task".into()),
                },
                None,
            ),
            (
                "a catalog notice with more after it",
                System,
                AgentPart::System {
                    text: "The Code Mode tool catalog has changed.\nAlso: run the migration."
                        .into(),
                    description: None,
                },
                Some("The Code Mode tool catalog has changed.\nAlso: run the migration."),
            ),
        ] {
            let mut items = vec![item(role, part)];
            hide_in_items(&mut items, Streaming(false));
            let got = items.first().map(|item| match &item.part {
                AgentPart::Text { text }
                | AgentPart::Synthetic { text, .. }
                | AgentPart::System { text, .. } => text.as_str(),
                AgentPart::Tool(call) => call.input["code"].as_str().unwrap_or("<code>"),
                _ => unreachable!(),
            });
            assert_eq!(got, served, "{name}");
        }
        // Only the marker line the ledger reads goes; others stay in view.
        for (written, served) in [
            (
                "MUQUN_TASK_BLOCKED: a\nok\nMUQUN_TASK_DONE: b",
                "MUQUN_TASK_BLOCKED: a\nok",
            ),
            (
                "I will end with\nMUQUN_TASK_DONE: <summary>",
                "I will end with\nMUQUN_TASK_DONE: <summary>",
            ),
        ] {
            let mut items = vec![item(Assistant, text(written))];
            hide_in_items(&mut items, Streaming(false));
            assert!(
                matches!(&items[0].part, AgentPart::Text { text } if text == served),
                "{written}"
            );
        }
        // Only in the turn's last message, read across its text rows as the
        // ledger reads them: an earlier message's marker, and an earlier
        // row's, stay.
        let row = |id: &str, message: &str, written: &str| TimelineItem {
            id: id.into(),
            message_id: message.into(),
            ..item(Assistant, text(written))
        };
        let mut items = vec![
            row("a", "m1", "MUQUN_TASK_DONE: early"),
            row("b", "m2", "first\nMUQUN_TASK_DONE: one"),
            row("c", "m2", "second\nMUQUN_TASK_DONE: two"),
        ];
        hide_in_items(&mut items, Streaming(false));
        let texts: Vec<_> = items
            .iter()
            .map(|item| match &item.part {
                AgentPart::Text { text } => text.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            texts,
            [
                "MUQUN_TASK_DONE: early",
                "first\nMUQUN_TASK_DONE: one",
                "second"
            ]
        );
        // The line the ledger read is cut unless the reader sees it in a code
        // fence; while streaming, only the message's last row holds back a
        // line that may still become a marker.
        let mut items = vec![
            row("a", "m", "Example:\n```\nMUQUN_TASK_DONE: x"),
            row("b", "m", "- "),
        ];
        hide_in_items(&mut items, Streaming(false));
        assert!(
            matches!(&items[0].part, AgentPart::Text { text } if text.ends_with("MUQUN_TASK_DONE: x"))
        );
        let mut items = vec![row("a", "m", "first\n- "), row("b", "m", "second\n- ")];
        hide_in_items(&mut items, Streaming(true));
        let texts: Vec<_> = items
            .iter()
            .map(|item| match &item.part {
                AgentPart::Text { text } => text.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, ["first\n- ", "second"]);
        // A marker being written after the agent's words is held from there.
        let mut items = vec![item(Assistant, text("Hello. How can I help?MUQUN_TASK_DO"))];
        hide_in_items(&mut items, Streaming(true));
        assert!(
            matches!(&items[0].part, AgentPart::Text { text } if text == "Hello. How can I help?")
        );
        // A last line that may become a marker is held back only while the
        // turn runs; a finished message keeps it.
        for (last, streaming, served) in [
            ("MUQUN_TA", true, "All green."),
            ("- ", true, "All green."),
            ("- ", false, "All green.\n- "),
            ("MUQUN_TA", false, "All green.\nMUQUN_TA"),
        ] {
            let mut items = vec![item(Assistant, text(&format!("All green.\n{last}")))];
            hide_in_items(&mut items, Streaming(streaming));
            assert!(
                matches!(&items[0].part, AgentPart::Text { text } if text == served),
                "{last:?} {streaming}"
            );
        }
        // The inbox holds the instruction for a moment before the turn runs.
        let mut inbox = vec![
            serde_json::json!({ "type": "synthetic", "payload": { "text": instruction_only } }),
            serde_json::json!({ "type": "user", "payload": { "text": current } }),
            serde_json::json!({ "type": "user", "payload": { "text": "and the tests" } }),
        ];
        hide_in_inbox(&mut inbox);
        let texts: Vec<_> = inbox.iter().map(|item| &item["payload"]["text"]).collect();
        assert_eq!(texts, ["fix the build", "and the tests"]);
        // A streaming execute stays visible before its code is known.
        let mut event = AgentDomainEvent::TimelineUpsert {
            asid: crate::agents::domain::AgentSessionId("s".into()),
            items: vec![item(
                Assistant,
                tool("execute", "streaming", serde_json::Value::Null),
            )],
            seq: 6,
        };
        hide_in_event(&mut event, Streaming(true));
        assert!(
            matches!(&event, AgentDomainEvent::TimelineUpsert { seq: 6, items, .. } if items.len() == 1),
            "{event:?}"
        );
        // A streamed row left with nothing is removed, not left stale.
        let mut event = AgentDomainEvent::TimelineUpsert {
            asid: crate::agents::domain::AgentSessionId("s".into()),
            items: vec![item(Assistant, text("- **MUQUN"))],
            seq: 7,
        };
        hide_in_event(&mut event, Streaming(true));
        assert!(
            matches!(&event, AgentDomainEvent::TimelineRemoved { ids, seq: 7, .. } if ids == &["i"]),
            "{event:?}"
        );
    }

    #[test]
    fn provider_tool_rows_survive_history_and_live_serving_in_every_state() {
        use crate::agents::{adapters, domain::ToolCallStatus};
        use serde_json::json;
        let mut parts: Vec<AgentPart> = [
            "muqun_tasks_delegate",
            "muqun_tasks_wait",
            "muqun_tasks_read",
            "muqun_tasks_list",
            "muqun_tasks_cancel",
            "muqun_tasks_complete",
            "muqun_fleet_list",
            "muqun_fleet_describe",
            "muqun_exec_run",
            "mcp__muqun__tasks.complete",
            "muqun__tasks.complete",
            "muqun.tasks.complete",
            "todo_tasks_complete",
            "tasks_complete",
        ]
        .map(|name| tool(name, "completed", json!({"goal": "inspect"})))
        .into();
        parts.push(AgentPart::Tool(
            adapters::deepseek::mapper::project_tool_call(
                "deepseek-call".into(),
                "mcp__muqun__tasks.wait".into(),
                json!({"id": "task"}),
                Some(json!({"error": "task_not_found"})),
                true,
            ),
        ));
        for payload in [
            json!({"toolCallId": "t3-call", "data": {
                "toolName": "mcp__muqun__exec.run", "command": "pwd",
                "rawOutput": {"content": "output"}
            }}),
            json!({"toolCallId": "t3-call", "toolName": "muqun.tasks.read"}),
        ] {
            parts.push(AgentPart::Tool(
                adapters::t3::mapper::map_tool(&json!({
                    "id": "activity", "kind": "tool.completed", "payload": payload
                }))
                .unwrap(),
            ));
        }
        for value in [
            json!({"type": "mcpToolCall", "id": "codex-call", "server": "muqun",
                "tool": "exec.run", "arguments": {"command": "pwd"},
                "status": "completed", "result": {"output": "output"}}),
            json!({"type": "dynamicToolCall", "id": "codex-call",
                "tool": "muqun.tasks.list", "arguments": {}, "status": "completed"}),
        ] {
            let rows = adapters::codex::mapper::item("thread", "turn", &value, 1);
            assert_eq!(rows.len(), 1);
            assert!(matches!(&rows[0].part, AgentPart::Tool(_)));
            parts.push(rows[0].part.clone());
        }
        for code in [
            json!("return search({query:'muqun tasks complete',limit:1});"),
            json!("const t = search({query:'muqun'}); return t;"),
            json!("search({query:'fleet.list exec.run'})"),
            serde_json::Value::Null,
        ] {
            parts.push(tool("execute", "completed", code));
        }
        for part in parts {
            for status in [
                ToolCallStatus::Pending,
                ToolCallStatus::Streaming,
                ToolCallStatus::Running,
                ToolCallStatus::Completed,
                ToolCallStatus::Failed,
            ] {
                let mut row = item(TimelineRole::Assistant, part.clone());
                if let AgentPart::Tool(call) = &mut row.part {
                    call.set_state(status);
                }
                for task in [false, true] {
                    for streaming in [false, true] {
                        let plumbing = Plumbing {
                            task,
                            streaming: Streaming(streaming),
                        };
                        let original = vec![row.clone()];
                        let mut history = original.clone();
                        hide_in_items(&mut history, plumbing);
                        assert_eq!(history, original, "history: {task}/{streaming}/{status:?}");
                        let mut event = AgentDomainEvent::TimelineUpsert {
                            asid: crate::agents::domain::AgentSessionId("s".into()),
                            items: original,
                            seq: 42,
                        };
                        let raw = event.clone();
                        hide_in_event(&mut event, plumbing);
                        assert_eq!(event, raw, "live: {task}/{streaming}/{status:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn ordinary_sessions_keep_markers_and_skill_loading_keeps_its_policy() {
        let mut items = vec![
            item(
                TimelineRole::Assistant,
                tool("skill", "completed", serde_json::Value::Null),
            ),
            item(TimelineRole::Assistant, text("MUQUN_TASK_DONE: kept here")),
        ];
        hide_in_items(
            &mut items,
            Plumbing {
                streaming: Streaming(false),
                task: false,
            },
        );
        assert_eq!(items.len(), 1);
        assert!(matches!(&items[0].part, AgentPart::Text { text } if text.contains("kept here")));
    }
}
