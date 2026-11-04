//! The turn summary bar: the last item of a finished turn, one monospace
//! line -- `3 files · +42 −7 · tests ✓ · 1m 12s` -- and `Diff ›`.
//!
//! Every field is read from what the turn holds, and a field the turn cannot
//! source is left out, never guessed: files and lines from the turn's Diff
//! parts (else the `files` metadata of its edit calls), tests from shell
//! runs whose command is a test command, the duration from the first to the
//! last timestamp in the turn. When `get_vcs_diff` has answered for this
//! session (`bindings.diff`), its file and line counts replace the turn's.

use std::collections::BTreeSet;
use std::sync::Arc;

use blinc_layout::prelude::*;
use muqun_gateway::agents::{AgentPart, TimelineItem, TimelineRole, ToolCall, ToolCallStatus};
use serde_json::Value;

use super::{mono, scale, PartCtx};
use crate::model::commands::Command;
use crate::ui::text::ui_text;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: Option<usize>,
    /// Lines added and removed, together or not at all.
    pub lines: Option<(u32, u32)>,
    /// Whether the last test run passed.
    pub tests: Option<bool>,
    pub millis: Option<u64>,
}

impl Summary {
    /// Whether there are changes to show with `Diff ›`.
    pub fn has_changes(&self) -> bool {
        self.files.is_some() || self.lines.is_some()
    }
}

/// Added and removed lines of a unified diff (file headers excluded).
pub fn count_lines(diff: &str) -> (u32, u32) {
    diff.lines().fold((0, 0), |(a, d), l| {
        if l.starts_with('+') && !l.starts_with("+++") {
            (a + 1, d)
        } else if l.starts_with('-') && !l.starts_with("---") {
            (a, d + 1)
        } else {
            (a, d)
        }
    })
}

/// The changes the edit calls of the turn report in `metadata.files`
/// (OpenCode's `FileDiff.Info`: `file`, `additions`, `deletions`).
fn call_changes(call: &ToolCall) -> Vec<(String, u32, u32)> {
    let Some(Value::Array(files)) = call.metadata.as_ref().and_then(|m| m.get("files")) else {
        return Vec::new();
    };
    files
        .iter()
        .filter_map(|f| {
            let name = f
                .get("file")
                .or_else(|| f.get("path"))
                .and_then(Value::as_str)?;
            let n = |k: &str| f.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            Some((name.to_string(), n("additions"), n("deletions")))
        })
        .collect()
}

const TEST_COMMANDS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "npm test",
    "npm run test",
    "pnpm test",
    "pnpm run test",
    "yarn test",
    "bun test",
    "pytest",
    "go test",
    "jest",
    "vitest",
    "mvn test",
    "gradle test",
    "gradlew test",
    "make test",
    "dotnet test",
    "rspec",
    "phpunit",
    "swift test",
    "flutter test",
    "ctest",
    "python -m unittest",
    "python3 -m unittest",
    "node --test",
    "deno test",
];

/// Whether a shell command runs a test suite.
pub fn is_test_command(command: &str) -> bool {
    let command = command.to_lowercase();
    TEST_COMMANDS.iter().any(|t| {
        command.match_indices(t).any(|(at, _)| {
            let before = command[..at].chars().next_back();
            let after = command[at + t.len()..].chars().next();
            before.is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_')
                && after.is_none_or(|c| !c.is_alphanumeric() && c != '_')
        })
    })
}

fn command_of(call: &ToolCall) -> Option<String> {
    ["command", "cmd", "script"]
        .iter()
        .find_map(|k| call.input.get(k).and_then(Value::as_str))
        .map(str::to_string)
        .or_else(|| call.title.clone())
}

/// How a finished shell run went: `Some(passed)`; `None` while unfinished.
fn outcome(call: &ToolCall) -> Option<bool> {
    match call.state {
        ToolCallStatus::Failed => Some(false),
        ToolCallStatus::Completed => {
            let exit = call
                .metadata
                .as_ref()
                .and_then(|m| m.get("exit"))
                .and_then(Value::as_f64);
            Some(exit.is_none_or(|c| c == 0.0))
        }
        _ => None,
    }
}

/// The summary of the turn made of `items`; `None` when the turn did no work
/// (a plain reply has nothing to summarise).
pub fn summarize(items: &[Arc<TimelineItem>]) -> Option<Summary> {
    let mut worked = false;
    let mut diff_files = BTreeSet::new();
    let mut diff_lines: Option<(u32, u32)> = None;
    let mut call_files = BTreeSet::new();
    let mut call_lines: Option<(u32, u32)> = None;
    let mut tests = None;
    let (mut first, mut last) = (u64::MAX, 0u64);
    let mut stamp = |ms: Option<u64>| {
        if let Some(ms) = ms.filter(|ms| *ms > 0) {
            first = first.min(ms);
            last = last.max(ms);
        }
    };
    for item in items {
        stamp(Some(item.updated_ms));
        if item.role == TimelineRole::User {
            continue;
        }
        match &item.part {
            AgentPart::Diff { file, diff } => {
                worked = true;
                if !file.is_empty() {
                    diff_files.insert(file.clone());
                }
                let (a, d) = count_lines(diff);
                let (ta, td) = diff_lines.unwrap_or((0, 0));
                diff_lines = Some((ta + a, td + d));
            }
            AgentPart::Tool(call) => {
                worked = true;
                stamp(call.time.created);
                stamp(call.time.ran);
                stamp(call.time.completed);
                for (file, a, d) in call_changes(call) {
                    call_files.insert(file);
                    let (ta, td) = call_lines.unwrap_or((0, 0));
                    call_lines = Some((ta + a, td + d));
                }
                if let Some(passed) = command_of(call)
                    .filter(|c| is_test_command(c))
                    .and_then(|_| outcome(call))
                {
                    tests = Some(passed);
                }
            }
            AgentPart::Shell {
                command,
                exit,
                status,
                ..
            } => {
                worked = true;
                if is_test_command(command) {
                    let failed = exit.is_some_and(|c| c != 0.0)
                        || matches!(status.as_str(), "failed" | "error");
                    let done = exit.is_some()
                        || matches!(status.as_str(), "completed" | "done" | "failed" | "error");
                    if done {
                        tests = Some(!failed);
                    }
                }
            }
            _ => {}
        }
    }
    if !worked {
        return None;
    }
    let (files, lines) = if diff_lines.is_some() {
        (
            (!diff_files.is_empty()).then_some(diff_files.len()),
            diff_lines,
        )
    } else {
        (
            (!call_files.is_empty()).then_some(call_files.len()),
            call_lines,
        )
    };
    let millis = (last > first && first != u64::MAX).then(|| last - first);
    let summary = Summary {
        files,
        lines,
        tests,
        millis,
    };
    (summary != Summary::default()).then_some(summary)
}

/// `42s`, `1m 12s`.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}s", s.max(1))
    }
}

/// Prefer the live working-tree diff when it is for this session.
fn with_vcs(summary: &Summary, ctx: &PartCtx<'_>) -> Summary {
    let Some(diff) = ctx.app.bindings.diff.get() else {
        return summary.clone();
    };
    if diff.asid != ctx.asid {
        return summary.clone();
    }
    if diff.files == 0 && diff.added == 0 && diff.removed == 0 {
        return summary.clone();
    }
    let mut out = summary.clone();
    out.files = Some(diff.files);
    out.lines = Some((diff.added as u32, diff.removed as u32));
    out
}

/// The bar. `Diff ›` opens the diff panel when there are changes to show.
pub fn bar(ctx: &PartCtx<'_>, summary: &Summary) -> Div {
    let summary = with_vcs(summary, ctx);
    let strings = &ctx.app.strings;
    let sep = || mono("·").class("mq-text-subtle");
    let mut parts: Vec<Div> = Vec::new();
    if let Some(n) = summary.files {
        parts.push(div().child(mono(strings.files(n)).class("mq-text-muted")));
    }
    if let Some((a, d)) = summary.lines {
        parts.push(
            div()
                .flex_row()
                .gap_px(6.0)
                .child(mono(format!("+{a}")).class("mq-text-success"))
                .child(mono(format!("−{d}")).class("mq-text-error")),
        );
    }
    if let Some(passed) = summary.tests {
        parts.push(
            div()
                .flex_row()
                .gap_px(4.0)
                .child(mono("tests").class("mq-text-muted"))
                .child(mono(if passed { "✓" } else { "✗" }).class(if passed {
                    "mq-text-success"
                } else {
                    "mq-text-error"
                })),
        );
    }
    if let Some(ms) = summary.millis {
        parts.push(div().child(mono(clock(ms)).class("mq-text-muted")));
    }
    let mut line = div()
        .flex_row()
        .items_center()
        .gap_px(8.0)
        .flex_1()
        .min_w(0.0);
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            line = line.child(sep());
        }
        line = line.child(part);
    }
    let mut row = div()
        .class("mq-summary")
        .w_full()
        .flex_row()
        .items_center()
        .child(line);
    if summary.has_changes() {
        let app = ctx.app.clone();
        let asid = muqun_gateway::agents::AgentSessionId(ctx.asid.to_string());
        row = row
            .id(ctx.key("summary"))
            .cursor_pointer()
            .child(
                ui_text(format!("{} ›", strings.diff()))
                    .size(scale::FS_12)
                    .no_wrap()
                    .no_cursor()
                    .class("mq-text-muted"),
            )
            .on_click(move |_| {
                app.send(Command::OpenDiff {
                    asid: asid.clone(),
                    file: None,
                })
            });
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use muqun_gateway::agents::TimelineRole;
    use serde_json::json;

    fn item(id: &str, role: TimelineRole, part: AgentPart, ms: u64) -> Arc<TimelineItem> {
        Arc::new(TimelineItem {
            id: id.into(),
            message_id: "m".into(),
            role,
            part,
            seq: 0,
            updated_ms: ms,
            ordinal: 0,
            attachments: None,
            image_assets: None,
            generation: None,
            origin: None,
        })
    }

    fn user(ms: u64) -> Arc<TimelineItem> {
        item(
            "u",
            TimelineRole::User,
            AgentPart::Text { text: "go".into() },
            ms,
        )
    }

    fn diff(file: &str, body: &str, ms: u64) -> Arc<TimelineItem> {
        item(
            file,
            TimelineRole::Assistant,
            AgentPart::Diff {
                file: file.into(),
                diff: body.into(),
            },
            ms,
        )
    }

    fn shell(id: &str, command: &str, exit: f64, ms: u64) -> Arc<TimelineItem> {
        item(
            id,
            TimelineRole::Assistant,
            AgentPart::Shell {
                shell_id: id.into(),
                command: command.into(),
                status: "completed".into(),
                exit: Some(exit),
                output: None,
                truncated: false,
            },
            ms,
        )
    }

    fn bash(id: &str, command: &str, state: &str, exit: Option<i64>) -> Arc<TimelineItem> {
        let call: ToolCall = serde_json::from_value(json!({
            "id": id, "name": "bash", "input": {"command": command},
            "state": state, "status": state,
            "metadata": exit.map(|e| json!({"exit": e})),
        }))
        .unwrap();
        item(id, TimelineRole::Assistant, AgentPart::Tool(call), 0)
    }

    #[test]
    fn a_reply_without_work_has_no_summary() {
        let reply = item(
            "a",
            TimelineRole::Assistant,
            AgentPart::Text { text: "hi".into() },
            9000,
        );
        assert_eq!(summarize(&[user(1000), reply]), None);
    }

    #[test]
    fn diffs_aggregate_by_file_and_line() {
        let items = [
            user(1000),
            diff("a.rs", "--- a\n+++ b\n@@\n+x\n+y\n-z\n", 2000),
            diff("b.rs", "+q\n", 3000),
            diff("a.rs", "-w\n", 4000),
        ];
        let s = summarize(&items).unwrap();
        assert_eq!(s.files, Some(2));
        assert_eq!(s.lines, Some((3, 2)));
        assert_eq!(s.millis, Some(3000));
        assert_eq!(s.tests, None);
    }

    #[test]
    fn edit_metadata_is_the_fallback_and_never_double_counts() {
        let call: ToolCall = serde_json::from_value(json!({
            "id": "e", "name": "edit", "input": {}, "state": "completed", "status": "completed",
            "metadata": {"files": [{"file": "x.rs", "additions": 4, "deletions": 1}]},
        }))
        .unwrap();
        let edit = item("e", TimelineRole::Assistant, AgentPart::Tool(call), 0);
        let s = summarize(&[user(0), edit.clone()]).unwrap();
        assert_eq!((s.files, s.lines), (Some(1), Some((4, 1))));
        let both = summarize(&[user(0), edit, diff("x.rs", "+a\n", 0)]).unwrap();
        assert_eq!(both.lines, Some((1, 0)), "diff parts win over metadata");
    }

    #[test]
    fn tests_follow_the_last_test_command() {
        let s = summarize(&[
            user(0),
            shell("1", "cargo test -p x", 1.0, 0),
            shell("2", "ls", 1.0, 0),
            shell("3", "cargo test -p x", 0.0, 0),
        ])
        .unwrap();
        assert_eq!(s.tests, Some(true));
        let s = summarize(&[user(0), bash("1", "pnpm test", "failed", None)]).unwrap();
        assert_eq!(s.tests, Some(false));
        let s = summarize(&[user(0), bash("1", "pytest -q", "completed", Some(2))]).unwrap();
        assert_eq!(s.tests, Some(false));
        let s = summarize(&[user(0), bash("1", "ls", "completed", Some(0))]);
        assert_eq!(s, None, "no field to show, no bar");
    }

    #[test]
    fn unfinished_test_runs_say_nothing() {
        let s = summarize(&[user(0), bash("1", "cargo test", "running", None)]);
        assert_eq!(s, None);
    }

    #[test]
    fn test_commands_match_words_not_substrings() {
        assert!(is_test_command("cd x && cargo test --workspace"));
        assert!(is_test_command("PYTHONPATH=. pytest -q"));
        assert!(!is_test_command("cargo testify"));
        assert!(!is_test_command("echo contest"));
    }

    #[test]
    fn durations_read_as_a_clock() {
        assert_eq!(clock(72_000), "1m 12s");
        assert_eq!(clock(4_000), "4s");
        assert_eq!(clock(400), "1s");
    }
}
