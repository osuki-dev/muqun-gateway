//! The task plumbing a session transcript is served without.
//!
//! A delegation asks its agent to say how it ended
//! ([`pane_agent::completion_instruction`]), and the agent may answer with a
//! marker line ([`pane_agent::DONE_MARKER`], [`pane_agent::BLOCKED_MARKER`]).
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
//! - an assistant text or reasoning without its marker lines (a line outside
//!   a code fence that starts, after bullets and markup, with a marker), and
//!   dropped when nothing else is left. A streamed text is served as
//!   snapshots of the whole text so far, so while the session runs a turn
//!   a last line that could only be the start of a marker (bare markup such
//!   as `- ` included) is held back too: the marker never flashes before
//!   its turn ends, and a finished message is never cut but for markers. A streamed row left with nothing is removed
//!   (`agent.timeline.removed`) rather than left out, so a client never keeps
//!   an older version of it.
//!
//! Sessions no task is bound to are served as they are. The mirror, the
//! agent's own reads and the task follower keep the raw text: detection
//! (`pane_agent::marker`, `service::turn_end`) reads what the agent wrote.

use super::pane_agent::{
    BLOCKED_MARKER, DONE_MARKER, REASON_PLACEHOLDER, REQUEST_OPENING, SUMMARY_PLACEHOLDER,
};
use crate::agents::domain::{AgentDomainEvent, AgentPart, TimelineItem, TimelineRole};
use crate::AppState;

/// How session `asid`'s transcript is served: `None` as it is, no task
/// being (or having been) bound to it; else without the plumbing, holding
/// back a last line still being written when it is `streaming` (running a
/// turn).
pub(crate) async fn for_session(state: &AppState, asid: &str) -> Option<Streaming> {
    if !state.tasks.is_task_session(asid).await {
        return None;
    }
    Some(Streaming(state.tasks.is_busy(asid)))
}

/// Whether the session is running a turn: its rows' last lines may still
/// be being written. A finished message is never cut but for its markers.
#[derive(Clone, Copy)]
pub(crate) struct Streaming(pub(crate) bool);

/// `items` as the reader sees them.
pub(crate) fn hide_in_items(items: &mut Vec<TimelineItem>, streaming: Streaming) {
    items.retain_mut(|item| hide_in_item(item, streaming) != Served::Emptied);
}

#[derive(PartialEq)]
enum Served {
    Kept,
    Emptied,
}

fn hide_in_item(item: &mut TimelineItem, streaming: Streaming) -> Served {
    let (text, served) = match (item.role, &mut item.part) {
        (TimelineRole::User, AgentPart::Text { text }) | (_, AgentPart::Synthetic { text, .. }) => {
            let served = without_request(text);
            (text, served)
        }
        (TimelineRole::Assistant, AgentPart::Text { text })
        | (TimelineRole::Assistant, AgentPart::Reasoning { text, .. }) => {
            let served = without_markers(text, streaming);
            (text, served)
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
pub(crate) fn hide_in_event(event: &mut AgentDomainEvent, streaming: Streaming) {
    match event {
        AgentDomainEvent::TimelineUpsert { asid, items, seq } => {
            let mut emptied = Vec::new();
            items.retain_mut(|item| {
                let kept = hide_in_item(item, streaming) == Served::Kept;
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
        AgentDomainEvent::InboxChanged { items, .. } => hide_in_inbox(items),
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

/// `text` without its marker lines, if it has any (or, `streaming`, ends in
/// what may be the start of one). Lines inside a code fence are the agent
/// quoting, and stay.
pub(crate) fn without_markers(text: &str, streaming: Streaming) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let mut fenced = false;
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    for (at, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
        }
        if fenced || !is_marker_line(line, streaming.0 && at == last) {
            kept.push(line);
        }
    }
    (kept.len() < lines.len()).then(|| kept.join("\n").trim_end().to_owned())
}

/// Whether `line` is a marker line -- or, as the last line still being
/// `written`, could become one: a marker's start, or bare list or emphasis
/// markup (not a `---` rule).
fn is_marker_line(line: &str, written: bool) -> bool {
    let body = line.trim_start_matches(|c: char| !c.is_alphanumeric());
    let markers = [DONE_MARKER, BLOCKED_MARKER];
    if markers.iter().any(|marker| body.starts_with(marker)) {
        return true;
    }
    if !written {
        return false;
    }
    if !body.is_empty() {
        return markers.iter().any(|marker| marker.starts_with(body));
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
    use crate::tasks::pane_agent::completion_request;

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
        }
    }

    fn text(text: &str) -> AgentPart {
        AgentPart::Text { text: text.into() }
    }

    #[test]
    fn the_reader_gets_the_goal_and_the_answer_without_the_plumbing() {
        use TimelineRole::{Assistant, System, User};
        let current = completion_request("fix the build");
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
        ] {
            let mut items = vec![item(role, part)];
            hide_in_items(&mut items, Streaming(false));
            let got = items.first().map(|item| match &item.part {
                AgentPart::Text { text } | AgentPart::Synthetic { text, .. } => text.as_str(),
                _ => unreachable!(),
            });
            assert_eq!(got, served, "{name}");
        }
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
}
