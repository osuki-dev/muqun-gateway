//! The task plumbing a session transcript is served without.
//!
//! A delegation asks its agent to say how it ended
//! ([`pane_agent::completion_instruction`]), and the agent may answer with a
//! marker line ([`pane_agent::DONE_MARKER`], [`pane_agent::BLOCKED_MARKER`]).
//! Both are how the gateway learns the task's end; the reader only needs the
//! goal and the answer. So on the way out (snapshot, timeline, event replay,
//! the SSE stream and `/api/ws`, like `message_images`) a transcript is served
//! with:
//!
//! - a user text (or a synthetic message) that ends with a completion
//!   instruction -- any wording this gateway has sent, found by its fixed
//!   opening and its closing placeholder line, not by the goal -- cut back to
//!   what came before it, and dropped when nothing did;
//! - an assistant text without its marker lines (a line that starts, after
//!   bullets and markup, with a marker), and dropped when nothing else is left.
//!   A streamed text is served as snapshots of the whole text so far, so a
//!   last line still being written that could only be the start of a marker
//!   is held back too: the marker never flashes before its turn ends.
//!
//! The mirror, the agent's own reads and the task follower keep the raw text:
//! detection (`pane_agent::marker`, `service::turn_end`) reads what the agent
//! wrote.

use super::pane_agent::{
    BLOCKED_MARKER, DONE_MARKER, REASON_PLACEHOLDER, REQUEST_OPENING, SUMMARY_PLACEHOLDER,
};
use crate::agents::domain::{AgentDomainEvent, AgentPart, TimelineItem, TimelineRole};

/// `items` as the reader sees them.
pub(crate) fn hide_in_items(items: &mut Vec<TimelineItem>) {
    items.retain_mut(|item| {
        let (text, served) = match (item.role, &mut item.part) {
            (TimelineRole::User, AgentPart::Text { text })
            | (_, AgentPart::Synthetic { text, .. }) => {
                let served = without_request(text);
                (text, served)
            }
            (TimelineRole::Assistant, AgentPart::Text { text }) => {
                let served = without_markers(text);
                (text, served)
            }
            _ => return true,
        };
        match served {
            Some(served) => {
                *text = served;
                !text.trim().is_empty()
            }
            None => true,
        }
    });
}

/// `event` as the reader sees it.
pub(crate) fn hide_in_event(event: &mut AgentDomainEvent) {
    if let AgentDomainEvent::TimelineUpsert { items, .. } = event {
        hide_in_items(items);
    }
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

/// `text` without its marker lines, if it has any (or ends in what may be
/// the start of one).
fn without_markers(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|&(at, line)| {
            let body = line.trim_start_matches(|c: char| !c.is_alphanumeric());
            let marker = [DONE_MARKER, BLOCKED_MARKER].iter().any(|marker| {
                body.starts_with(marker)
                    || (at == last && !body.is_empty() && marker.starts_with(body))
            });
            !marker
        })
        .map(|(_, line)| *line)
        .collect();
    (kept.len() < lines.len()).then(|| kept.join("\n").trim_end().to_owned())
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
                "marker still streaming",
                Assistant,
                text("All green.\n\nMUQUN_TA"),
                Some("All green."),
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
        ] {
            let mut items = vec![item(role, part)];
            hide_in_items(&mut items);
            let got = items.first().map(|item| match &item.part {
                AgentPart::Text { text } | AgentPart::Synthetic { text, .. } => text.as_str(),
                _ => unreachable!(),
            });
            assert_eq!(got, served, "{name}");
        }
    }
}
