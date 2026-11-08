//! How the timeline folds the agent's tool activity: consecutive tool calls
//! (and the thinking between them) read as one summary line, `Read 6 files
//! · ran 12 commands`, so the agent's words stay the main thing on screen.
//! What needs the owner, or carries a picture, stays a row of its own.

use muqun_gateway::agents::{AgentPart, TimelineItem, ToolCall, ToolCallStatus};

use super::tool_view::{self as tv, ToolKind};

/// One row of the timeline list: an item, or a run of activity items
/// `start..end` (two or more) folded under a summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Item(usize),
    Group { start: usize, end: usize },
}

/// A tool call's family, as the summary counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Read,
    Edit,
    Run,
    Search,
    Web,
    Subagent,
    Other,
    /// Thinking between calls: folded with them, not counted.
    Thought,
}

/// The kind of a call, as the timeline draws it (ACP names a shell call by
/// its category, `execute`).
pub fn tool_kind(call: &ToolCall) -> ToolKind {
    let kind = tv::classify(&call.name);
    if kind == ToolKind::Execute
        && tv::input_record(call).is_some_and(|r| r.contains_key("command"))
    {
        ToolKind::Shell
    } else {
        kind
    }
}

fn family_of(kind: ToolKind) -> Family {
    match kind {
        ToolKind::Read => Family::Read,
        ToolKind::Edit | ToolKind::Write | ToolKind::Patch => Family::Edit,
        ToolKind::Shell => Family::Run,
        ToolKind::Glob | ToolKind::Grep | ToolKind::Search => Family::Search,
        ToolKind::Web | ToolKind::Browser => Family::Web,
        ToolKind::Subagent => Family::Subagent,
        _ => Family::Other,
    }
}

/// The family an item folds as, or `None` when it stands on its own: words,
/// approvals, questions, a checklist, presented files, anything with a picture.
pub fn family(item: &TimelineItem) -> Option<Family> {
    match &item.part {
        AgentPart::Reasoning { .. } => Some(Family::Thought),
        AgentPart::Shell { .. } => Some(Family::Run),
        AgentPart::Diff { .. } => Some(Family::Edit),
        AgentPart::Tool(call) => {
            if let Some(m) = tv::muqun(call) {
                return m.files.is_empty().then_some(Family::Other);
            }
            let kind = tool_kind(call);
            if matches!(kind, ToolKind::Question | ToolKind::Todo) {
                return None;
            }
            if !super::media::tool_image_refs(call).is_empty() {
                return None;
            }
            Some(family_of(kind))
        }
        _ => None,
    }
}

/// An item that draws nothing: a text part with no words and no picture
/// (Codex leaves these between turns).
pub fn invisible(item: &TimelineItem) -> bool {
    match &item.part {
        AgentPart::Text { text } => {
            text.trim().is_empty()
                && item.attachments.as_ref().is_none_or(Vec::is_empty)
                && item.image_assets.as_ref().is_none_or(Vec::is_empty)
        }
        _ => false,
    }
}

/// The list's rows: runs of two or more activity items fold into a group
/// (invisible items inside a run do not break it); everything else is its
/// own row. Invisible items get no row.
pub fn blocks(items: &[TimelineItem]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut run: Option<(usize, usize, usize)> = None; // start, end, members
    let flush = |run: &mut Option<(usize, usize, usize)>, out: &mut Vec<Block>| {
        if let Some((start, end, members)) = run.take() {
            if members >= 2 {
                out.push(Block::Group { start, end });
            } else {
                // One call: its own row (skipping any invisible tail).
                if let Some(i) = (start..end).find(|&i| !invisible(&items[i])) {
                    out.push(Block::Item(i));
                }
            }
        }
    };
    for (i, item) in items.iter().enumerate() {
        if invisible(item) {
            if let Some(r) = run.as_mut() {
                r.1 = i + 1;
            }
            continue;
        }
        if family(item).is_some() {
            match run.as_mut() {
                Some(r) => {
                    r.1 = i + 1;
                    r.2 += 1;
                }
                None => run = Some((i, i + 1, 1)),
            }
            continue;
        }
        flush(&mut run, &mut out);
        out.push(Block::Item(i));
    }
    flush(&mut run, &mut out);
    out
}

/// What a group did, for its summary line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub read: usize,
    pub edited: usize,
    pub ran: usize,
    pub searched: usize,
    pub web: usize,
    pub subagents: usize,
    pub other: usize,
    pub failed: usize,
    /// The last call still running, if any.
    pub running: Option<usize>,
}

/// Whether a call ended badly: failed, or a command that exited non-zero.
pub fn failed(item: &TimelineItem) -> bool {
    match &item.part {
        AgentPart::Tool(call) => {
            if call.state == ToolCallStatus::Failed {
                return true;
            }
            if tool_kind(call) == ToolKind::Shell {
                let out = tv::output_text(call);
                return tv::shell_exit(call, &out).is_some_and(|c| c != 0)
                    || tv::shell_timed_out(call);
            }
            false
        }
        AgentPart::Shell { exit, status, .. } => {
            status != "running" && status != "pending" && exit.is_some_and(|e| e != 0.0)
        }
        _ => false,
    }
}

/// Whether a call is still under way (as the row says; a stopped session's
/// pending call is the caller's to word).
pub fn running(item: &TimelineItem) -> bool {
    match &item.part {
        AgentPart::Tool(call) => tv::pending(call.state),
        AgentPart::Shell { status, .. } => status == "running" || status == "pending",
        _ => false,
    }
}

/// Count `items[start..end]` for the group's summary. Edits count files.
pub fn summarize(items: &[TimelineItem]) -> Summary {
    let mut s = Summary::default();
    let mut edited: Vec<String> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(family) = family(item) else {
            continue;
        };
        match family {
            Family::Read => s.read += 1,
            Family::Run => s.ran += 1,
            Family::Search => s.searched += 1,
            Family::Web => s.web += 1,
            Family::Subagent => s.subagents += 1,
            Family::Other => s.other += 1,
            Family::Thought => {}
            Family::Edit => {
                let path = match &item.part {
                    AgentPart::Diff { file, .. } => file.clone(),
                    AgentPart::Tool(call) => {
                        tv::target(tool_kind(call), tv::input_record(call).as_ref())
                    }
                    _ => String::new(),
                };
                if path.is_empty() || !edited.contains(&path) {
                    edited.push(path);
                }
            }
        }
        if failed(item) {
            s.failed += 1;
        }
        if running(item) {
            s.running = Some(i);
        }
    }
    s.edited = edited.len();
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::TimelineRole;
    use serde_json::json;

    fn item(id: &str, part: serde_json::Value) -> TimelineItem {
        serde_json::from_value(json!({
            "id": id, "message_id": id, "role": "assistant", "seq": 0, "updated_ms": 0,
            "ordinal": 0, "part": part,
        }))
        .unwrap()
    }

    fn tool(
        id: &str,
        name: &str,
        input: serde_json::Value,
        extra: serde_json::Value,
    ) -> TimelineItem {
        let mut part = json!({ "type": "tool", "id": id, "name": name, "input": input,
            "state": "completed", "status": "completed" });
        for (k, v) in extra.as_object().unwrap() {
            part[k] = v.clone();
        }
        item(id, part)
    }

    fn text(id: &str, body: &str) -> TimelineItem {
        let mut it = item(id, json!({ "type": "text", "text": body }));
        it.role = TimelineRole::Assistant;
        it
    }

    #[test]
    fn runs_of_calls_fold_and_words_and_owner_asks_break_them() {
        let items = vec![
            text("t0", "Looking."),
            tool("r1", "Read", json!({"file_path": "a.rs"}), json!({})),
            item("th", json!({ "type": "reasoning", "text": "hm" })),
            text("blank", "  "),
            tool("s1", "bash", json!({"command": "ls"}), json!({})),
            text("t1", "Now a question."),
            tool("q", "question", json!({"questions": []}), json!({})),
            tool("s2", "bash", json!({"command": "ls"}), json!({})),
            tool(
                "img",
                "view_image",
                json!({"path": "/tmp/x.png"}),
                json!({}),
            ),
            text("t2", "Done."),
        ];
        assert_eq!(
            blocks(&items),
            [
                Block::Item(0),
                Block::Group { start: 1, end: 5 },
                Block::Item(5),
                Block::Item(6),
                // A lone call is its own row; a call with a picture stands apart.
                Block::Item(7),
                Block::Item(8),
                Block::Item(9),
            ]
        );
    }

    #[test]
    fn the_summary_counts_files_once_and_surfaces_failures() {
        let items = vec![
            tool("e1", "edit", json!({"file_path": "a.rs"}), json!({})),
            tool("e2", "edit", json!({"file_path": "a.rs"}), json!({})),
            tool("w1", "write", json!({"file_path": "b.rs"}), json!({})),
            tool(
                "s1",
                "bash",
                json!({"command": "cargo test"}),
                json!({"output": "boom", "metadata": {"exit": 101}}),
            ),
            tool(
                "s2",
                "bash",
                json!({"command": "ls"}),
                json!({"state": "running", "status": "running"}),
            ),
            tool("g1", "grep", json!({"pattern": "x"}), json!({})),
        ];
        let s = summarize(&items);
        assert_eq!((s.edited, s.ran, s.searched), (2, 2, 1));
        assert_eq!(s.failed, 1);
        assert_eq!(s.running, Some(4));
    }
}
