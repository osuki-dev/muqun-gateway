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
//! - an assistant text or reasoning without the one marker line the ledger
//!   reads its end from ([`pane_agent::consumed_marker_line`], shared with
//!   detection: an earlier marker line, one in a code fence or one the
//!   ledger ignores stays in view), and dropped when nothing else is left.
//!   A streamed text is served as snapshots of the whole text so far, so
//!   while the session runs a turn a last line that could only be the start
//!   of a marker (bare markup such as `- ` included) is held back too: the
//!   marker never flashes before its turn ends;
//! - OpenCode's Code Mode plumbing: an `execute` call whose whole code is a
//!   catalog lookup of a muqun tool ([`muqun_lookup`], parsed strictly; one
//!   that failed stays), held while its code is not known yet, and the
//!   "Code Mode tool catalog has changed" notice when it is exactly that.
//!
//! A streamed row left with nothing is removed (`agent.timeline.removed`)
//! rather than left out, so a client never keeps an older version of it.
//!
//! Sessions no task is bound to are served as they are. The mirror, the
//! agent's own reads and the task follower keep the raw text: detection
//! (`pane_agent::marker`, `service::turn_end`) reads what the agent wrote.

use super::pane_agent::{
    self, BLOCKED_MARKER, DONE_MARKER, REASON_PLACEHOLDER, REQUEST_OPENING, SUMMARY_PLACEHOLDER,
};
use crate::agents::domain::{
    AgentDomainEvent, AgentPart, TimelineItem, TimelineRole, ToolCallStatus,
};
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
        (_, AgentPart::System { text, .. }) | (_, AgentPart::Synthetic { text, .. })
            if is_catalog_notice(text) =>
        {
            return Served::Emptied;
        }
        (TimelineRole::User, AgentPart::Text { text }) | (_, AgentPart::Synthetic { text, .. }) => {
            let served = without_request(text);
            (text, served)
        }
        (TimelineRole::Assistant, AgentPart::Text { text })
        | (TimelineRole::Assistant, AgentPart::Reasoning { text, .. }) => {
            let served = without_markers(text, streaming);
            (text, served)
        }
        (_, AgentPart::Tool(call)) if call.name == "execute" => {
            // Held until its code is known, then shown unless it is only a
            // muqun lookup that did not fail.
            let code = call.input.get("code").and_then(|code| code.as_str());
            let unknown = matches!(
                call.status,
                ToolCallStatus::Pending | ToolCallStatus::Streaming
            );
            let failed = call.status == ToolCallStatus::Failed || call.error.is_some();
            let lookup = !failed && code.is_some_and(muqun_lookup);
            return if unknown || lookup {
                Served::Emptied
            } else {
                Served::Kept
            };
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

/// The first line of OpenCode's notice that its Code Mode tool catalog
/// changed (the muqun tools joining it); the catalog listing follows it
/// under a `# Code Mode` heading.
const CATALOG_NOTICE: [&str; 2] = [
    "The Code Mode tool catalog has changed.",
    "The Code Mode tool catalog has changed. This catalog supersedes the previous Code Mode tool catalog.",
];

/// Whether `text` is exactly that notice: its first line, then nothing or
/// the catalog listing.
fn is_catalog_notice(text: &str) -> bool {
    let text = text.trim();
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let rest = rest.trim_start();
    CATALOG_NOTICE.contains(&first.trim()) && (rest.is_empty() || rest.starts_with("# Code Mode\n"))
}

/// Whether `code` (an OpenCode Code Mode `execute` call) is only a catalog
/// lookup of a muqun tool, read strictly as the whole code:
/// `[return] search({query: <string>[, limit: <integer>]})[;]`, keys in any
/// order, each once, the query a plain quoted string (no escapes, no
/// template) that starts with `muqun` or names `tasks.`, `fleet.` or
/// `exec.run`. Anything else -- another statement, a chained call, an
/// operator, another key -- is the agent's own work and is shown.
fn muqun_lookup(code: &str) -> bool {
    let mut at = Cursor(code.trim());
    if at.word("return") && !at.0.starts_with(char::is_whitespace) {
        return false;
    }
    let (mut query, mut limit) = (None, false);
    let parsed = at.eat("search")
        && at.eat("(")
        && at.eat("{")
        && loop {
            let key = at.key();
            if !at.eat(":") {
                break false;
            }
            match key {
                Some("query") if query.is_none() => match at.string() {
                    Some(text) => query = Some(text),
                    None => break false,
                },
                Some("limit") if !limit && at.integer() => limit = true,
                _ => break false,
            }
            let comma = at.eat(",");
            if at.eat("}") {
                break true;
            }
            if !comma {
                break false;
            }
        }
        && at.eat(")");
    at.eat(";");
    let ours = |query: &str| {
        let query = query.trim().to_ascii_lowercase();
        query.starts_with("muqun")
            || ["tasks.", "fleet.", "exec.run"]
                .iter()
                .any(|id| query.contains(id))
    };
    parsed && at.0.trim().is_empty() && query.is_some_and(ours)
}

/// A tiny reader over the rest of a code text, skipping whitespace before
/// each token.
struct Cursor<'a>(&'a str);

impl<'a> Cursor<'a> {
    /// Takes `token` if the rest starts with it.
    fn eat(&mut self, token: &str) -> bool {
        let rest = self.0.trim_start();
        match rest.strip_prefix(token) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    /// Takes `word` if it is a whole word at the start.
    fn word(&mut self, word: &str) -> bool {
        let rest = self.0.trim_start();
        let whole = rest
            .strip_prefix(word)
            .is_some_and(|after| !after.starts_with(|c: char| c.is_alphanumeric() || c == '_'));
        if whole {
            self.0 = &rest[word.len()..];
        }
        whole
    }

    /// An object key: a name, or one in plain quotes.
    fn key(&mut self) -> Option<&'a str> {
        let rest = self.0.trim_start();
        if rest.starts_with(['"', '\'']) {
            return self.string();
        }
        let end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        (end > 0).then(|| {
            self.0 = &rest[end..];
            &rest[..end]
        })
    }

    /// A string literal in `'` or `"` without escapes.
    fn string(&mut self) -> Option<&'a str> {
        let rest = self.0.trim_start();
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
        let body = &rest[1..];
        let end = body.find(quote)?;
        let text = &body[..end];
        if text.contains(['\\', '\n']) {
            return None;
        }
        self.0 = &body[end + 1..];
        Some(text)
    }

    /// An integer literal.
    fn integer(&mut self) -> bool {
        let rest = self.0.trim_start();
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if end == 0 {
            return false;
        }
        self.0 = &rest[end..];
        true
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

/// `text` without the marker line the ledger reads its end from
/// ([`pane_agent::consumed_marker_line`], the same rule detection uses: so
/// an earlier marker line, one in a code fence, or one the ledger ignores
/// stays in view) -- and, `streaming`, without a last line that may still
/// become a marker.
pub(crate) fn without_markers(text: &str, streaming: Streaming) -> Option<String> {
    let consumed = pane_agent::consumed_marker_line(text);
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len() - 1;
    let fenced_at_last = lines[..last]
        .iter()
        .filter(|line| pane_agent::is_fence(line))
        .count()
        % 2
        == 1;
    let held = streaming.0 && !fenced_at_last && may_become_marker(lines[last]);
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|&(at, _)| Some(at) != consumed && !(held && at == last))
        .map(|(_, line)| *line)
        .collect();
    (kept.len() < lines.len()).then(|| kept.join("\n").trim_end().to_owned())
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

    /// An OpenCode tool call `name`, `status`, with `code` as its input.
    fn tool(name: &str, status: &str, code: serde_json::Value) -> AgentPart {
        AgentPart::Tool(crate::agents::adapters::opencode::mapper::map_tool_call(
            &serde_json::json!({ "type": "tool", "id": "c", "name": name,
                "state": { "status": status, "input": { "code": code } } }),
        ))
    }

    /// A finished Code Mode `execute` call running `code`.
    fn execute(code: &str) -> AgentPart {
        tool("execute", "completed", code.into())
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
            (
                "a catalog lookup for a muqun tool",
                Assistant,
                execute("return search({query:'muqun tasks complete',limit:1});"),
                None,
            ),
            (
                "a lookup by tool id",
                Assistant,
                execute(r#"search({ "query": "tasks.complete" })"#),
                None,
            ),
            (
                "a script that also looks one up",
                Assistant,
                execute("const t = search({query:'muqun'});\nreturn t;"),
                Some("const t = search({query:'muqun'});\nreturn t;"),
            ),
            (
                "a lookup of another tool",
                Assistant,
                execute("return search({query:'browser tabs'});"),
                Some("return search({query:'browser tabs'});"),
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
                "a catalog notice with more after it",
                System,
                AgentPart::System {
                    text: "The Code Mode tool catalog has changed.\nAlso: run the migration."
                        .into(),
                    description: None,
                },
                Some("The Code Mode tool catalog has changed.\nAlso: run the migration."),
            ),
            (
                "a fleet lookup",
                Assistant,
                execute("return search({ limit: 3, query: \"fleet.self\" })"),
                None,
            ),
            (
                "an exec.run lookup",
                Assistant,
                execute("search({'query':'exec.run'});"),
                None,
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
        // Real work shaped like a lookup is shown; a lookup still being
        // written is held until its code is known.
        for (name, part, hidden) in [
            (
                "chained",
                execute("search({query:'muqun'}).then(r => tools.fleet.exec({}))"),
                false,
            ),
            (
                "and-ed",
                execute("search({query:'muqun'}) && await tools.bash.run('x')"),
                false,
            ),
            (
                "work before the query",
                execute("search({limit: await tools.x(), query:'muqun'})"),
                false,
            ),
            (
                "a template query",
                execute("search({query: `muqun${await run()}`})"),
                false,
            ),
            (
                "a built query",
                execute("search({query:'muqun'+await run()})"),
                false,
            ),
            (
                "an extra key",
                execute("search({query:'muqun', all: true})"),
                false,
            ),
            (
                "search in a string",
                execute("log('search({query:\\'muqun\\'})')"),
                false,
            ),
            (
                "another statement",
                execute("search({query:'muqun'}); rm()"),
                false,
            ),
            (
                "return without semicolon",
                execute("return search({query:'muqun tasks'})"),
                true,
            ),
            (
                "another tool",
                tool("bash", "completed", "search({query:'muqun'})".into()),
                false,
            ),
            (
                "code not a string",
                tool("execute", "completed", serde_json::json!(1)),
                false,
            ),
            (
                "a failed lookup",
                tool("execute", "error", "search({query:'muqun'})".into()),
                false,
            ),
            (
                "still streaming",
                tool("execute", "streaming", serde_json::Value::Null),
                true,
            ),
        ] {
            let mut items = vec![item(Assistant, part)];
            hide_in_items(&mut items, Streaming(true));
            assert_eq!(items.is_empty(), hidden, "{name}");
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
